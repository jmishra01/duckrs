// Ported from duckdb/src/common/box_renderer.cpp @ rust-port-baseline
//
//! `JSONParser` / `JSONFormatter`: pretty-printing a nested or JSON value over several lines when its
//! column is too narrow for it.

use super::unicode::render_width;

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsonState {
    Regular,
    InQuote,
    Escape,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormattingMode {
    Standard,
    CompactVertical,
    CompactHorizontal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ComponentType {
    BracketOpen,
    BracketClose,
    Literal,
    Colon,
    Comma,
    NullValue,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormattingResult {
    Success,
    TooManyRows,
    TooWide,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineMode {
    Standard,
    InlinedSingleLine,
    InlinedMultiLine,
}

struct Component {
    ty: ComponentType,
    text: String,
}

struct FormatState {
    mode: FormattingMode,
    result: String,
    component_idx: usize,
    row_count: usize,
    line_length: usize,
    depth: usize,
    max_rows: usize,
    max_width: usize,
    indentation_size: usize,
    format_result: FormattingResult,
}

/// `JSONFormatter`.
#[derive(Default)]
struct JsonFormatter {
    components: Vec<Component>,
    /// the bytes of the literal being built (literals only split at ASCII characters)
    literal: Vec<u8>,
}

/// `JSONFormatter::FormatValue`: `text` pretty-printed in at most `max_rows` extra lines of
/// `max_width`, or `None` if it can't be.
pub(crate) fn format_value(text: &str, max_rows: usize, max_width: usize) -> Option<String> {
    let mut formatter = JsonFormatter::default();
    // C++ ignores whether parsing succeeded: whatever components were found are formatted
    formatter.process(text.as_bytes());
    let mut indentation_size = 2;
    let result = match formatter.try_format(FormattingMode::Standard, max_rows, max_width, indentation_size) {
        Ok(text) => return Some(text),
        Err(result) => result,
    };
    let mode = if result == FormattingResult::TooWide {
        indentation_size = 1;
        FormattingMode::CompactHorizontal
    } else {
        FormattingMode::CompactVertical
    };
    formatter.try_format(mode, max_rows, max_width, indentation_size).ok()
}

fn is_whitespace_escape(c: u8) -> bool {
    c == b'n' || c == b't'
}

impl JsonFormatter {
    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            let bytes = std::mem::take(&mut self.literal);
            let text = String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            self.components.push(Component {
                ty: ComponentType::Literal,
                text,
            });
        }
    }

    fn push(&mut self, ty: ComponentType, text: &str) {
        self.flush_literal();
        self.components.push(Component { ty, text: text.into() });
    }

    fn add_literal_character(&mut self, c: u8) {
        self.literal.push(c);
    }

    /// `JSONParser::Process`.
    fn process(&mut self, value: &[u8]) -> bool {
        let mut separators: Vec<u8> = Vec::new();
        let mut state = JsonState::Regular;
        let mut quote_char = b'"';
        let mut can_parse_value = false;
        let mut in_unquoted_value = false;
        let mut success = true;
        let mut pos = 0;
        while success && pos < value.len() {
            let c = value[pos];
            match state {
                JsonState::Regular => {
                    if can_parse_value
                        && pos + 4 < value.len()
                        && c.eq_ignore_ascii_case(&b'n')
                        && value[pos + 1].eq_ignore_ascii_case(&b'u')
                        && value[pos + 2].eq_ignore_ascii_case(&b'l')
                        && value[pos + 3].eq_ignore_ascii_case(&b'l')
                    {
                        self.push(ComponentType::NullValue, "null");
                        pos += 4;
                        continue;
                    }
                    match c {
                        b'[' | b'{' => {
                            separators.push(c);
                            self.push(ComponentType::BracketOpen, if c == b'[' { "[" } else { "{" });
                            can_parse_value = c == b'[';
                        }
                        b'}' | b']' => {
                            let matching = matches!((separators.last(), c), (Some(b'{'), b'}') | (Some(b'['), b']'));
                            if !matching {
                                success = false;
                            } else {
                                separators.pop();
                                self.push(ComponentType::BracketClose, if c == b']' { "]" } else { "}" });
                                in_unquoted_value = false;
                            }
                        }
                        b'"' | b'\'' => {
                            self.add_literal_character(c);
                            quote_char = c;
                            state = JsonState::InQuote;
                        }
                        b',' => {
                            self.push(ComponentType::Comma, ",");
                            in_unquoted_value = false;
                        }
                        b':' => {
                            self.push(ComponentType::Colon, ":");
                            can_parse_value = true;
                        }
                        b'\\' => {
                            // skip the literal "\n" and "\t" that rendering escaped before
                            if pos + 1 < value.len() && is_whitespace_escape(value[pos + 1]) {
                                pos += 1;
                            } else {
                                self.add_literal_character(c);
                            }
                        }
                        b' ' => {
                            if in_unquoted_value {
                                self.add_literal_character(c);
                            }
                        }
                        b'\t' | b'\n' => {}
                        _ => {
                            in_unquoted_value = true;
                            self.add_literal_character(c);
                        }
                    }
                }
                JsonState::InQuote => {
                    if c == quote_char {
                        state = JsonState::Regular;
                    } else if c == b'\\' {
                        state = JsonState::Escape;
                    }
                    self.add_literal_character(c);
                }
                JsonState::Escape => {
                    state = JsonState::InQuote;
                    self.add_literal_character(c);
                }
            }
            pos += 1;
        }
        self.flush_literal();
        success
    }

    fn literal_fits(state: &FormatState, width: usize) -> bool {
        state.line_length + width <= state.max_width
    }

    fn add_literal(state: &mut FormatState, text: &str, skip_adding_if_does_not_fit: bool) {
        let width = render_width(text);
        if !Self::literal_fits(state, width) {
            if skip_adding_if_does_not_fit {
                return;
            }
            Self::add_newline(state);
            if state.format_result != FormattingResult::Success {
                return;
            }
        }
        state.result.push_str(text);
        state.line_length += width;
        if state.line_length > state.max_width {
            state.format_result = FormattingResult::TooWide;
        }
    }

    fn add_space(state: &mut FormatState) {
        Self::add_literal(state, " ", true);
    }

    fn add_newline(state: &mut FormatState) {
        state.result.push('\n');
        state.result.extend(std::iter::repeat_n(' ', state.depth));
        state.row_count += 1;
        if state.row_count > state.max_rows {
            state.format_result = FormattingResult::TooManyRows;
            return;
        }
        state.line_length = state.depth;
        if state.line_length > state.max_width {
            state.format_result = FormattingResult::TooWide;
        }
    }

    fn is_separator(ty: ComponentType) -> bool {
        matches!(ty, ComponentType::Comma | ComponentType::Colon)
    }

    fn format_component(&self, state: &mut FormatState, idx: usize, inline_mode: InlineMode) {
        let components = &self.components;
        let component = &components[idx];
        let max_width = state.max_width;
        match component.ty {
            ComponentType::BracketOpen => {
                state.depth += if component.text == "{" {
                    state.indentation_size
                } else {
                    1
                };
                Self::add_literal(state, &component.text, false);
                if inline_mode != InlineMode::Standard {
                    return;
                }
                // look ahead to the matching bracket: can we inline it without exceeding the width?
                let c = state.component_idx;
                let mut peek_depth = 0usize;
                let mut render_size = state.line_length;
                let mut inline_child_mode = InlineMode::Standard;
                let mut peek_idx = c + 1;
                while peek_idx < components.len() && render_size <= max_width {
                    let peek = &components[peek_idx];
                    if peek.ty == ComponentType::BracketOpen {
                        peek_depth += 1;
                    } else if peek.ty == ComponentType::BracketClose {
                        if peek_depth == 0 {
                            if render_size + 1 < max_width {
                                inline_child_mode = InlineMode::InlinedSingleLine;
                            }
                            break;
                        }
                        peek_depth -= 1;
                    }
                    render_size += render_width(&peek.text);
                    if Self::is_separator(peek.ty) {
                        render_size += 1;
                    }
                    peek_idx += 1;
                }
                if component.text == "[" {
                    // arrays are inlined unless there are objects inside them
                    peek_idx = c + 1;
                    while peek_idx < components.len() {
                        let peek = &components[peek_idx];
                        if peek.ty == ComponentType::BracketOpen && peek.text == "{" {
                            break;
                        }
                        if peek.ty == ComponentType::BracketClose {
                            inline_child_mode = InlineMode::InlinedMultiLine;
                            break;
                        }
                        peek_idx += 1;
                    }
                }
                if inline_child_mode != InlineMode::Standard {
                    let peek_idx = peek_idx.min(components.len() - 1);
                    for inline_idx in c + 1..=peek_idx {
                        let inline_component = &components[inline_idx];
                        if inline_child_mode == InlineMode::InlinedMultiLine && inline_idx < peek_idx {
                            let next = &components[inline_idx + 1];
                            if matches!(next.ty, ComponentType::Comma | ComponentType::BracketClose) {
                                let combined = format!("{}{}", inline_component.text, next.text);
                                if !Self::literal_fits(state, render_width(&combined)) {
                                    Self::add_newline(state);
                                }
                            }
                        }
                        self.format_component(state, inline_idx, inline_child_mode);
                    }
                    state.component_idx = peek_idx;
                    return;
                }
                if state.mode == FormattingMode::CompactVertical
                    && c + 1 < components.len()
                    && components[c + 1].ty == ComponentType::BracketOpen
                {
                    // the next bracket adds the newline
                    return;
                }
                Self::add_newline(state);
            }
            ComponentType::BracketClose => {
                let depth_diff = if component.text == "}" {
                    state.indentation_size
                } else {
                    1
                };
                state.depth = state.depth.saturating_sub(depth_diff);
                if inline_mode == InlineMode::Standard {
                    Self::add_newline(state);
                }
                Self::add_literal(state, &component.text, false);
            }
            ComponentType::Comma | ComponentType::Colon => {
                Self::add_literal(state, &component.text, false);
                let always_inline = if state.mode == FormattingMode::CompactHorizontal {
                    false
                } else {
                    component.ty == ComponentType::Colon
                };
                if inline_mode != InlineMode::Standard || always_inline {
                    Self::add_space(state);
                    return;
                }
                if state.mode != FormattingMode::Standard {
                    // try to inline until the next comma
                    let c = state.component_idx;
                    let mut peek_depth = 0usize;
                    let mut render_size = state.line_length + 1;
                    let mut inline_comma = false;
                    let mut peek_idx = c + 1;
                    while peek_idx < components.len() && render_size <= max_width {
                        let peek = &components[peek_idx];
                        if peek.ty == ComponentType::BracketOpen {
                            peek_depth += 1;
                        } else if peek.ty == ComponentType::BracketClose {
                            if peek_depth == 0 {
                                inline_comma = render_size + 1 < max_width;
                                break;
                            }
                            peek_depth -= 1;
                        }
                        if peek_depth == 0 && peek.ty == ComponentType::Comma {
                            inline_comma = render_size + 2 <= max_width;
                            break;
                        }
                        render_size += render_width(&peek.text);
                        if Self::is_separator(peek.ty) {
                            render_size += 1;
                        }
                        peek_idx += 1;
                    }
                    if inline_comma {
                        Self::add_space(state);
                        for inline_idx in c + 1..peek_idx {
                            self.format_component(state, inline_idx, InlineMode::InlinedSingleLine);
                        }
                        state.component_idx = peek_idx - 1;
                        return;
                    }
                }
                Self::add_newline(state);
            }
            ComponentType::NullValue | ComponentType::Literal => {
                Self::add_literal(state, &component.text, false);
            }
        }
    }

    /// `JSONFormatter::TryFormat`.
    fn try_format(
        &self,
        mode: FormattingMode,
        max_rows: usize,
        max_width: usize,
        indentation_size: usize,
    ) -> Result<String, FormattingResult> {
        let mut state = FormatState {
            mode,
            result: String::new(),
            component_idx: 0,
            row_count: 0,
            line_length: 0,
            depth: 0,
            max_rows,
            max_width,
            indentation_size,
            format_result: FormattingResult::Success,
        };
        while state.component_idx < self.components.len() && state.format_result == FormattingResult::Success {
            let idx = state.component_idx;
            self.format_component(&mut state, idx, InlineMode::Standard);
            state.component_idx += 1;
        }
        if state.format_result != FormattingResult::Success {
            return Err(state.format_result);
        }
        Ok(state.result)
    }
}
