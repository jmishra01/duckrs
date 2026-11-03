// Ported from duckdb/src/common/types/uuid.cpp @ rust-port-baseline
//
// A UUID is stored as a HUGEINT whose top bit is flipped, so signed integer order matches the
// unsigned byte order of the UUID text.

const FLIP: u128 = 1 << 127;

/// Formats a stored UUID as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (`BaseUUID::ToString`).
pub fn uuid_to_string(stored: i128) -> String {
    let v = (stored as u128) ^ FLIP;
    let hex = format!("{v:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Parses a UUID (`BaseUUID::FromString`): 32 hex digits with any hyphens, optionally in
/// braces. Strict mode requires the canonical 36-character hyphenated form.
pub fn uuid_from_str(s: &str, strict: bool) -> Option<i128> {
    let data = s.as_bytes();
    if data.is_empty() {
        return None;
    }
    let braces = usize::from(data[0] == b'{');
    if braces == 1 && data[data.len() - 1] != b'}' {
        return None;
    }
    if strict && (data.len() != 36 || [8, 13, 18, 23].iter().any(|&i| data[i] != b'-')) {
        return None;
    }
    let mut value: u128 = 0;
    let mut count = 0;
    for &c in &data[braces..data.len() - braces] {
        if c == b'-' {
            continue;
        }
        let digit = (c as char).to_digit(16)?;
        if count >= 32 {
            return None;
        }
        value = (value << 4) | u128::from(digit);
        count += 1;
    }
    (count == 32).then_some((value ^ FLIP) as i128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for text in [
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ] {
            let v = uuid_from_str(text, true).unwrap();
            assert_eq!(uuid_to_string(v), text);
        }
        assert_eq!(
            uuid_from_str("00000000-0000-0000-0000-000000000000", true),
            Some(i128::MIN)
        );
        let braced = uuid_from_str("{6BA7B8109DAD11D180B400C04FD430C8}", false).unwrap();
        assert_eq!(uuid_to_string(braced), "6ba7b810-9dad-11d1-80b4-00c04fd430c8");
        assert!(uuid_from_str("6ba7b8109dad11d180b400c04fd430c8", true).is_none());
        assert!(uuid_from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c", false).is_none());
        assert!(uuid_from_str("6ba7b810-9dad-11d1-80b4-00c04fd430cg", false).is_none());
    }

    #[test]
    fn ordering_matches_text_order() {
        let a = uuid_from_str("00000000-0000-0000-0000-000000000001", true).unwrap();
        let b = uuid_from_str("80000000-0000-0000-0000-000000000000", true).unwrap();
        assert!(a < b);
    }
}
