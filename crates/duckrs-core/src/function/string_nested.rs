// Ported from duckdb/src/function/cast/vector_cast_helpers.cpp @ rust-port-baseline
//
//! Splits the VARCHAR form of a LIST, MAP or STRUCT into the strings of its parts. A part is
//! `None` for NULL. Every function gives `None` for text that is not of the form it expects.

use duckrs_common::string_util::is_space;

type Part = Option<Vec<u8>>;

struct InputState<'a> {
    buf: &'a [u8],
    pos: usize,
    escaped: bool,
}

/// `start` and `end` of the part being scanned.
#[derive(Default)]
struct Span {
    start: Option<usize>,
    end: usize,
}

impl Span {
    fn mark(&mut self, pos: usize) {
        self.start.get_or_insert(pos);
        self.end = pos;
    }
}

fn is_null(buf: &[u8], start: usize, end: usize) -> bool {
    end == start + 4 && buf[start..end].eq_ignore_ascii_case(b"null")
}

impl InputState<'_> {
    fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn peek(&self) -> u8 {
        self.buf[self.pos]
    }

    fn skip_whitespace(&mut self) {
        while !self.at_end() && is_space(self.peek()) {
            self.pos += 1;
            self.escaped = false;
        }
    }

    /// `SkipToCloseQuotes`.
    fn skip_to_close_quotes(&mut self) -> bool {
        let quote = self.peek();
        self.pos += 1;
        while !self.at_end() {
            let mut set_escaped = false;
            if self.peek() == b'\\' {
                set_escaped = !self.escaped;
            } else if self.peek() == quote && !self.escaped {
                return true;
            }
            self.escaped = set_escaped;
            self.pos += 1;
        }
        false
    }

    /// `SkipToClose`.
    fn skip_to_close(&mut self) -> bool {
        let mut brackets: Vec<u8> = Vec::new();
        while !self.at_end() {
            let c = self.peek();
            let mut set_escaped = false;
            if c == b'"' || c == b'\'' {
                if !self.escaped && !self.skip_to_close_quotes() {
                    return false;
                }
            } else if c == b'{' {
                brackets.push(b'}');
            } else if c == b'(' {
                brackets.push(b')');
            } else if c == b'[' {
                brackets.push(b']');
            } else if brackets.last() == Some(&c) {
                brackets.pop();
                if brackets.is_empty() {
                    return true;
                }
            } else if c == b'\\' {
                set_escaped = true;
            }
            self.escaped = set_escaped;
            self.pos += 1;
        }
        false
    }

    /// `ValueStateTransition`.
    fn value_transition(&mut self, span: &mut Span) -> bool {
        let c = self.peek();
        let mut set_escaped = false;
        if c == b'"' || c == b'\'' {
            span.start.get_or_insert(self.pos);
            if !self.escaped && !self.skip_to_close_quotes() {
                return false;
            }
            span.end = self.pos;
        } else if matches!(c, b'{' | b'(' | b'[') {
            span.start.get_or_insert(self.pos);
            if !self.skip_to_close() {
                return false;
            }
            span.end = self.pos;
        } else if c == b'\\' {
            span.mark(self.pos);
            set_escaped = true;
        } else if !is_space(c) {
            span.mark(self.pos);
        }
        self.escaped = set_escaped;
        self.pos += 1;
        true
    }

    /// Scans a value that ends at one of `ends`; `false` if the text ends first.
    fn value(&mut self, span: &mut Span, ends: &[u8]) -> bool {
        while !self.at_end() && !ends.contains(&self.peek()) {
            if !self.value_transition(span) {
                return false;
            }
        }
        !self.at_end()
    }
}

/// `HandleString`: removes the quotes and escapes of a part.
fn handle_string(buf: &[u8], start: usize, end: usize, respect_scopes: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(end - start);
    let (mut escaped, mut quoted, mut quote_char) = (false, false, 0);
    let mut scopes: Vec<u8> = Vec::new();
    for i in start..end {
        let c = buf[i];
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        if c == b'\\' && (quoted || (scopes.is_empty() && i + 1 < end && matches!(buf[i + 1], b'\'' | b'"'))) {
            escaped = true;
            if !scopes.is_empty() {
                out.push(c);
            }
            continue;
        }
        if c == b'\'' || c == b'"' {
            if quoted && c == quote_char {
                quoted = false;
                if scopes.is_empty() {
                    continue;
                }
            } else if !quoted {
                quoted = true;
                quote_char = c;
                if scopes.is_empty() {
                    continue;
                }
            }
        }
        if !quoted && scopes.last() == Some(&c) {
            scopes.pop();
        }
        if !quoted && respect_scopes {
            match c {
                b'[' => scopes.push(b']'),
                b'{' => scopes.push(b'}'),
                b'(' => scopes.push(b')'),
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// The part in `span` (NULL as `None`); a part with nothing in it is the empty string.
fn part(buf: &[u8], span: &Span) -> Part {
    let Some(start) = span.start else {
        return Some(Vec::new());
    };
    let end = span.end + 1;
    (!is_null(buf, start, end)).then(|| handle_string(buf, start, end, true))
}

/// `SplitStringListInternal`.
pub(super) fn split_list(buf: &[u8]) -> Option<Vec<Part>> {
    let mut state = InputState {
        buf,
        pos: 0,
        escaped: false,
    };
    let mut parts = Vec::new();
    state.skip_whitespace();
    if state.at_end() || state.peek() != b'[' {
        return None;
    }
    state.pos += 1;
    state.skip_whitespace();
    let mut seen_value = false;
    while !state.at_end() {
        let mut span = Span::default();
        if !state.value(&mut span, b",]") {
            return None;
        }
        if state.peek() != b']' || span.start.is_some() || seen_value {
            parts.push(part(buf, &span));
            seen_value = true;
        }
        if state.peek() == b']' {
            break;
        }
        state.pos += 1;
        state.skip_whitespace();
    }
    state.pos += 1;
    state.skip_whitespace();
    (state.pos == buf.len()).then_some(parts)
}

/// `SplitStringMapInternal`. The key of an entry is never NULL.
pub(super) fn split_map(buf: &[u8]) -> Option<Vec<(Vec<u8>, Part)>> {
    let mut state = InputState {
        buf,
        pos: 0,
        escaped: false,
    };
    let mut entries = Vec::new();
    state.skip_whitespace();
    if state.at_end() || state.peek() != b'{' {
        return None;
    }
    state.pos += 1;
    state.skip_whitespace();
    if state.at_end() {
        return None;
    }
    if state.peek() == b'}' {
        state.pos += 1;
        state.skip_whitespace();
        return (state.pos == buf.len()).then_some(entries);
    }
    while !state.at_end() {
        let mut span = Span::default();
        if !state.value(&mut span, b"=") {
            return None;
        }
        let key = part(buf, &span)?;
        state.pos += 1;
        state.skip_whitespace();
        let mut span = Span::default();
        if !state.value(&mut span, b",}") {
            return None;
        }
        entries.push((key, part(buf, &span)));
        if state.peek() == b'}' {
            break;
        }
        state.pos += 1;
        state.skip_whitespace();
    }
    state.pos += 1;
    state.skip_whitespace();
    (state.pos == buf.len()).then_some(entries)
}

/// `VectorStringToStruct::SplitStruct`: the parts of the fields called `names`, in that order. A
/// field the text leaves out is NULL.
pub(super) fn split_struct(buf: &[u8], names: &[String]) -> Option<Vec<Part>> {
    let mut state = InputState {
        buf,
        pos: 0,
        escaped: false,
    };
    let mut parts: Vec<Part> = vec![None; names.len()];
    state.skip_whitespace();
    if state.at_end() || !matches!(state.peek(), b'{' | b'(') {
        return None;
    }
    let end_char = if state.peek() == b'{' { b'}' } else { b')' };
    state.pos += 1;
    state.skip_whitespace();
    if state.at_end() {
        return None;
    }
    if state.peek() == end_char {
        state.pos += 1;
        state.skip_whitespace();
        return (state.pos == buf.len()).then_some(parts);
    }
    if end_char == b'}' {
        let mut seen = vec![false; names.len()];
        while !state.at_end() {
            let index = struct_key(&mut state, names)?;
            if std::mem::replace(&mut seen[index], true) {
                return None;
            }
            state.pos += 1;
            state.skip_whitespace();
            let mut span = Span::default();
            if !state.value(&mut span, b",}") {
                return None;
            }
            parts[index] = part(buf, &span);
            if state.peek() == b'}' {
                break;
            }
            state.pos += 1;
            state.skip_whitespace();
        }
    } else {
        let mut index = 0;
        while !state.at_end() {
            if index == names.len() {
                return None;
            }
            let mut span = Span::default();
            if !state.value(&mut span, b",)") {
                return None;
            }
            parts[index] = part(buf, &span);
            if state.peek() == b')' {
                break;
            }
            index += 1;
            state.pos += 1;
            state.skip_whitespace();
            if !state.at_end() && state.peek() == b')' {
                // a trailing comma, as in the single-element tuple "(1,)"
                break;
            }
        }
    }
    state.pos += 1;
    state.skip_whitespace();
    (state.pos == buf.len()).then_some(parts)
}

/// The key of a STRUCT entry, as the index of its field. Stops at the `:`.
fn struct_key(state: &mut InputState, names: &[String]) -> Option<usize> {
    let mut span = Span::default();
    while !state.at_end() && state.peek() != b':' {
        let c = state.peek();
        let mut set_escaped = false;
        if state.escaped {
            span.mark(state.pos);
        } else if c == b'"' || c == b'\'' {
            span.start.get_or_insert(state.pos);
            if !state.skip_to_close_quotes() {
                return None;
            }
            span.end = state.pos;
        } else if c == b'\\' {
            span.mark(state.pos);
            set_escaped = true;
        } else if !is_space(c) {
            span.mark(state.pos);
        }
        state.escaped = set_escaped;
        state.pos += 1;
    }
    if state.at_end() {
        return None;
    }
    let start = span.start?;
    let end = span.end + 1;
    if is_null(state.buf, start, end) {
        return None;
    }
    let key = handle_string(state.buf, start, end, false);
    let key = String::from_utf8_lossy(&key);
    names.iter().position(|n| n.eq_ignore_ascii_case(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(part: &Part) -> Option<String> {
        part.as_ref().map(|p| String::from_utf8_lossy(p).into_owned())
    }

    #[test]
    fn lists_split_like_duckdb() {
        let split = |s: &str| split_list(s.as_bytes()).map(|p| p.iter().map(text).collect::<Vec<_>>());
        let s = |v: &str| Some(v.to_string());
        assert_eq!(split("[]"), Some(vec![]));
        assert_eq!(split(" [ 1, 2 ] "), Some(vec![s("1"), s("2")]));
        assert_eq!(split("[NULL, 'NULL']"), Some(vec![None, s("NULL")]));
        assert_eq!(split("[[1, 2], [3]]"), Some(vec![s("[1, 2]"), s("[3]")]));
        assert_eq!(split("['a,b', \"c\"]"), Some(vec![s("a,b"), s("c")]));
        assert_eq!(split("[1, 2"), None);
        assert_eq!(split("1, 2]"), None);
        assert_eq!(split("[1] x"), None);
    }

    #[test]
    fn structs_split_like_duckdb() {
        let names = ["a".to_string(), "b".to_string()];
        let split = |s: &str| split_struct(s.as_bytes(), &names).map(|p| p.iter().map(text).collect::<Vec<_>>());
        let s = |v: &str| Some(v.to_string());
        assert_eq!(split("{a: 1, 'b': x}"), Some(vec![s("1"), s("x")]));
        assert_eq!(split("{b: 1}"), Some(vec![None, s("1")]));
        assert_eq!(split("(1, 2)"), Some(vec![s("1"), s("2")]));
        assert_eq!(split("{c: 1}"), None);
        assert_eq!(split("{a: 1, a: 2}"), None);
        assert_eq!(split("(1, 2, 3)"), None);
    }
}
