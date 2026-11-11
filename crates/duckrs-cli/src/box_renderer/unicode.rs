// Ported from duckdb/third_party/utf8proc/utf8proc_wrapper.cpp @ rust-port-baseline
//
//! The render widths and grapheme clusters the box renderer needs (`Utf8Proc::RenderWidth`,
//! `Utf8Proc::NextGraphemeCluster`).
//!
//! C++ reads both from utf8proc's Unicode tables. The crate has no Unicode-table dependency, so these
//! are approximations: a cluster is a base character plus the combining marks, variation selectors,
//! emoji modifiers and zero-width-joined characters after it (and a pair of regional indicators), and
//! a character is 2 wide when it is East Asian wide/fullwidth or an emoji, 0 wide when it is a control,
//! combining or format character, and 1 wide otherwise.

/// Whether `cp` joins the cluster before it (`Extend` / `SpacingMark` / ZWJ, roughly).
fn is_extend(cp: u32) -> bool {
    matches!(
        cp,
        0x0300..=0x036F
            | 0x0483..=0x0489
            | 0x0591..=0x05BD
            | 0x05BF
            | 0x05C1..=0x05C2
            | 0x05C4..=0x05C5
            | 0x05C7
            | 0x0610..=0x061A
            | 0x064B..=0x065F
            | 0x0670
            | 0x06D6..=0x06DC
            | 0x06DF..=0x06E4
            | 0x06E7..=0x06E8
            | 0x06EA..=0x06ED
            | 0x0711
            | 0x0730..=0x074A
            | 0x07A6..=0x07B0
            | 0x07EB..=0x07F3
            | 0x0816..=0x082D
            | 0x0859..=0x085B
            | 0x08D3..=0x0903
            | 0x093A..=0x094F
            | 0x0951..=0x0957
            | 0x0962..=0x0963
            | 0x0981..=0x0983
            | 0x09BC..=0x09D7
            | 0x09E2..=0x09E3
            | 0x0A01..=0x0A03
            | 0x0A3C..=0x0A51
            | 0x0A70..=0x0A71
            | 0x0A75
            | 0x0A81..=0x0A83
            | 0x0ABC..=0x0ACD
            | 0x0AE2..=0x0AE3
            | 0x0B01..=0x0B03
            | 0x0B3C..=0x0B57
            | 0x0B82
            | 0x0BBE..=0x0BCD
            | 0x0BD7
            | 0x0C00..=0x0C04
            | 0x0C3E..=0x0C56
            | 0x0C81..=0x0C83
            | 0x0CBC..=0x0CD6
            | 0x0D00..=0x0D03
            | 0x0D3B..=0x0D57
            | 0x0D81..=0x0D83
            | 0x0DCA..=0x0DF3
            | 0x0E31
            | 0x0E34..=0x0E3A
            | 0x0E47..=0x0E4E
            | 0x0EB1
            | 0x0EB4..=0x0EBC
            | 0x0EC8..=0x0ECD
            | 0x0F18..=0x0F19
            | 0x0F35
            | 0x0F37
            | 0x0F39
            | 0x0F3E..=0x0F3F
            | 0x0F71..=0x0F84
            | 0x0F86..=0x0F87
            | 0x0F8D..=0x0FBC
            | 0x102B..=0x103E
            | 0x1160..=0x11FF
            | 0x135D..=0x135F
            | 0x1712..=0x1714
            | 0x17B4..=0x17D3
            | 0x180B..=0x180D
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200C..=0x200D
            | 0x20D0..=0x20FF
            | 0x302A..=0x302F
            | 0x3099..=0x309A
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0xFF9E..=0xFF9F
            | 0x1F3FB..=0x1F3FF
            | 0xE0020..=0xE007F
            | 0xE0100..=0xE01EF
    )
}

fn is_regional_indicator(cp: u32) -> bool {
    (0x1F1E6..=0x1F1FF).contains(&cp)
}

/// `utf8proc_charwidth`.
pub(crate) fn char_width(cp: u32) -> usize {
    if cp < 0x20 || (0x7F..0xA0).contains(&cp) {
        return 0;
    }
    if is_extend(cp) || matches!(cp, 0x00AD | 0x200B | 0x200E..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0xFEFF) {
        return 0;
    }
    let wide = matches!(
        cp,
        0x1100..=0x115F
            | 0x231A..=0x231B
            | 0x2329..=0x232A
            | 0x23E9..=0x23EC
            | 0x23F0
            | 0x23F3
            | 0x25FD..=0x25FE
            | 0x2614..=0x2615
            | 0x2648..=0x2653
            | 0x267F
            | 0x2693
            | 0x26A1
            | 0x26AA..=0x26AB
            | 0x26BD..=0x26BE
            | 0x26C4..=0x26C5
            | 0x26CE
            | 0x26D4
            | 0x26EA
            | 0x26F2..=0x26F3
            | 0x26F5
            | 0x26FA
            | 0x26FD
            | 0x2705
            | 0x270A..=0x270B
            | 0x2728
            | 0x274C
            | 0x274E
            | 0x2753..=0x2755
            | 0x2757
            | 0x2795..=0x2797
            | 0x27B0
            | 0x27BF
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x16FE0..=0x16FE4
            | 0x17000..=0x18CFF
            | 0x1B000..=0x1B2FF
            | 0x1F004
            | 0x1F0CF
            | 0x1F18E
            | 0x1F191..=0x1F19A
            | 0x1F1E6..=0x1F1FF
            | 0x1F200..=0x1F2FF
            | 0x1F300..=0x1F320
            | 0x1F32D..=0x1F335
            | 0x1F337..=0x1F37C
            | 0x1F37E..=0x1F393
            | 0x1F3A0..=0x1F3CA
            | 0x1F3CF..=0x1F3D3
            | 0x1F3E0..=0x1F3F0
            | 0x1F3F4
            | 0x1F3F8..=0x1F43E
            | 0x1F440
            | 0x1F442..=0x1F4FC
            | 0x1F4FF..=0x1F53D
            | 0x1F54B..=0x1F54E
            | 0x1F550..=0x1F567
            | 0x1F57A
            | 0x1F595..=0x1F596
            | 0x1F5A4
            | 0x1F5FB..=0x1F64F
            | 0x1F680..=0x1F6C5
            | 0x1F6CC
            | 0x1F6D0..=0x1F6D2
            | 0x1F6D5..=0x1F6D7
            | 0x1F6DC..=0x1F6DF
            | 0x1F6EB..=0x1F6EC
            | 0x1F6F4..=0x1F6FC
            | 0x1F7E0..=0x1F7EB
            | 0x1F7F0
            | 0x1F90C..=0x1F93A
            | 0x1F93C..=0x1F945
            | 0x1F947..=0x1F9FF
            | 0x1FA70..=0x1FAFF
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD
    );
    if wide { 2 } else { 1 }
}

/// `Utf8Proc::NextGraphemeCluster`: the byte position after the cluster that starts at `pos`.
pub(crate) fn next_grapheme_cluster(s: &str, pos: usize) -> usize {
    let mut chars = s[pos..].char_indices();
    let Some((_, first)) = chars.next() else {
        return s.len();
    };
    let mut end = pos + first.len_utf8();
    let first = first as u32;
    if first == u32::from('\r') {
        if s.as_bytes().get(end) == Some(&b'\n') {
            end += 1;
        }
        return end;
    }
    if first < 0x20 || (0x7F..0xA0).contains(&first) {
        return end;
    }
    let mut prev = first;
    let mut regional_pair = is_regional_indicator(first);
    for (_, ch) in chars {
        let cp = ch as u32;
        let joins = is_extend(cp) || prev == 0x200D || (regional_pair && is_regional_indicator(cp));
        if !joins {
            break;
        }
        if regional_pair && is_regional_indicator(cp) {
            regional_pair = false;
        }
        end += ch.len_utf8();
        prev = cp;
    }
    end
}

/// `Utf8Proc::RenderWidth(s, len, pos)`: the width of the character at `pos`.
pub(crate) fn render_width_at(s: &str, pos: usize) -> usize {
    s[pos..].chars().next().map_or(0, |c| char_width(c as u32))
}

/// `Utf8Proc::RenderWidth(str)`: the width of each cluster's first character, summed.
pub(crate) fn render_width(s: &str) -> usize {
    let mut width = 0;
    let mut pos = 0;
    while pos < s.len() {
        width += render_width_at(s, pos);
        pos = next_grapheme_cluster(s, pos);
    }
    width
}
