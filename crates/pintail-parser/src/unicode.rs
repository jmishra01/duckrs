// Ported from duckdb/src/parser/parser.cpp (Parser::StripUnicodeSpaces) @ rust-port-baseline

/// Replaces Unicode space characters outside quotes, dollar-quoted strings and `--` comments
/// with the same number of ASCII spaces, so byte offsets stay aligned with the input. Returns
/// `None` when there is nothing to replace.
///
/// Handled: U+00A0, U+2000..U+200B, U+202F, U+205F, U+2060, U+3000 and U+FEFF. Like the C++
/// scan, the last two bytes of the query are not inspected.
pub fn strip_unicode_spaces(query: &str) -> Option<String> {
    let q = query.as_bytes();
    let n = q.len();
    let mut spaces: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    let first_tag_char = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let tag_char = |c: u8| first_tag_char(c) || c.is_ascii_digit();
    'regular: loop {
        while pos + 2 < n {
            match q[pos] {
                0xC2 if q[pos + 1] == 0xA0 => spaces.push((pos, 2)),
                0xE2 if q[pos + 1] == 0x80 && ((0x80..=0x8B).contains(&q[pos + 2]) || q[pos + 2] == 0xAF) => {
                    spaces.push((pos, 3))
                }
                0xE2 if q[pos + 1] == 0x81 && (q[pos + 2] == 0x9F || q[pos + 2] == 0xA0) => spaces.push((pos, 3)),
                0xE3 if q[pos + 1] == 0x80 && q[pos + 2] == 0x80 => spaces.push((pos, 3)),
                0xEF if q[pos + 1] == 0xBB && q[pos + 2] == 0xBF => spaces.push((pos, 3)),
                b'"' | b'\'' => {
                    let quote = q[pos];
                    pos += 1;
                    while pos + 1 < n {
                        if q[pos] == quote {
                            if q[pos + 1] == quote {
                                pos += 2;
                                continue;
                            }
                            pos += 1;
                            continue 'regular;
                        }
                        pos += 1;
                    }
                    break 'regular;
                }
                b'$' if q[pos + 1] == b'$' || first_tag_char(q[pos + 1]) => {
                    pos += 1;
                    let start = pos;
                    let mut tag = None;
                    while pos + 2 < n {
                        if q[pos] == b'$' {
                            tag = Some(&q[start..pos]);
                            break;
                        }
                        if !tag_char(q[pos]) {
                            continue 'regular;
                        }
                        pos += 1;
                    }
                    let Some(tag) = tag else { break 'regular };
                    while pos + 2 < n {
                        if q[pos] == b'$'
                            && n - (pos + 1) > tag.len()
                            && q[pos + tag.len() + 1] == b'$'
                            && &q[pos + 1..pos + 1 + tag.len()] == tag
                        {
                            pos += tag.len() + 1;
                            continue 'regular;
                        }
                        pos += 1;
                    }
                    break 'regular;
                }
                b'-' if q[pos + 1] == b'-' => {
                    while pos < n {
                        if q[pos] == b'\n' || q[pos] == b'\r' {
                            continue 'regular;
                        }
                        pos += 1;
                    }
                    break 'regular;
                }
                _ => {}
            }
            pos += 1;
        }
        break;
    }
    if spaces.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(n);
    let mut prev = 0;
    for (start, len) in spaces {
        out.push_str(&query[prev..start]);
        out.extend(std::iter::repeat_n(' ', len));
        prev = start + len;
    }
    out.push_str(&query[prev..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_spaces_outside_quotes() {
        assert_eq!(strip_unicode_spaces("SELECT\u{a0}42;").as_deref(), Some("SELECT  42;"));
        assert_eq!(
            strip_unicode_spaces("SELECT\u{3000}'\u{a0}'\u{feff}1;").as_deref(),
            Some("SELECT   '\u{a0}'   1;")
        );
        assert_eq!(strip_unicode_spaces("SELECT 42"), None);
        assert_eq!(strip_unicode_spaces("SELECT $x$\u{a0}$x$ ;"), None);
        assert_eq!(strip_unicode_spaces("SELECT 1 -- \u{a0}\n;"), None);
    }
}
