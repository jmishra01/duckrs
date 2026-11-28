// Ported from duckdb/src/parser/keyword_helper.cpp (SQLIdentifier::ToString) @ rust-port-baseline

use std::sync::OnceLock;

/// Whether a word is a keyword of the grammar, which the parser crate knows (`KeywordHelper::IsKeyword`).
static KEYWORD_CHECK: OnceLock<fn(&str) -> bool> = OnceLock::new();

/// Gives [`sql_identifier`] the keywords of the grammar: the engine does this when it starts.
pub fn set_keyword_check(check: fn(&str) -> bool) {
    let _ = KEYWORD_CHECK.set(check);
}

fn is_keyword(text: &str) -> bool {
    KEYWORD_CHECK.get().is_some_and(|check| check(text))
}

/// Renders an identifier, double-quoting it when it is not a plain lowercase/uppercase word, or is a keyword
/// once [`set_keyword_check`] says what keywords are.
pub fn sql_identifier(text: &str) -> String {
    let bytes = text.as_bytes();
    let ambiguous_after_dot = bytes.len() > 1 && bytes[0] == b'_' && bytes[1].is_ascii_digit();
    let plain = bytes
        .iter()
        .enumerate()
        .all(|(i, &c)| (i > 0 && c.is_ascii_digit()) || c.is_ascii_lowercase() || c.is_ascii_uppercase() || c == b'_');
    if !text.is_empty() && !ambiguous_after_dot && plain && !is_keyword(text) {
        return text.to_string();
    }
    format!("\"{}\"", text.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sql_identifier("abc_1"), "abc_1");
        assert_eq!(sql_identifier("Abc"), "Abc");
        assert_eq!(sql_identifier("1abc"), "\"1abc\"");
        assert_eq!(sql_identifier("my col"), "\"my col\"");
        assert_eq!(sql_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(sql_identifier(""), "\"\"");
        assert_eq!(sql_identifier("_1"), "\"_1\"");
    }
}
