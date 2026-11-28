// Ported from duckdb/src/common/types/bit.cpp @ rust-port-baseline
//
//! BIT values. The first byte counts the padding bits, which are the high bits of the second
//! byte and are all 1. The bits follow, most significant first.

fn padding(bits: &[u8]) -> usize {
    bits[0] as usize
}

/// `Bit::BitLength`.
pub fn bit_length(bits: &[u8]) -> usize {
    (bits.len() - 1) * 8 - padding(bits)
}

/// `Bit::GetBit`.
pub fn get_bit(bits: &[u8], n: usize) -> bool {
    let n = n + padding(bits);
    bits[n / 8 + 1] >> (7 - n % 8) & 1 != 0
}

/// A BIT value of these bits.
pub fn bits_to_bit(values: impl ExactSizeIterator<Item = bool>) -> Vec<u8> {
    let length = values.len();
    let padding = (8 - length % 8) % 8;
    let mut out = vec![0u8; length.div_ceil(8) + 1];
    out[0] = padding as u8;
    for position in 0..padding {
        out[1] |= 1 << (7 - position);
    }
    for (i, value) in values.enumerate() {
        if value {
            let n = i + padding;
            out[n / 8 + 1] |= 1 << (7 - n % 8);
        }
    }
    out
}

/// `Bit::ToString`.
pub fn bit_to_string(bits: &[u8]) -> String {
    (0..bit_length(bits))
        .map(|i| if get_bit(bits, i) { '1' } else { '0' })
        .collect()
}

fn invalid_character(c: u8) -> String {
    format!(
        "Invalid character encountered in string -> bit conversion: '{}'",
        c as char
    )
}

/// `Bit::ToBit`, also for the `x...` hex form: the error is a conversion error's message.
pub fn string_to_bit(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.is_empty() {
        return Ok(bits_to_bit(std::iter::once(false)));
    }
    if input[0] == b'x' {
        let digits = &input[1..];
        if digits.is_empty() {
            return Err("Cannot cast empty string to BIT".to_string());
        }
        let mut out = Vec::with_capacity(digits.len() / 2 + 2);
        out.push(if digits.len().is_multiple_of(2) { 0 } else { 4 });
        let hex = |c: u8| {
            (c as char)
                .to_digit(16)
                .map(|d| d as u8)
                .ok_or_else(|| invalid_character(c))
        };
        let mut pairs = digits;
        if !digits.len().is_multiple_of(2) {
            out.push(hex(digits[0])? | 0xF0);
            pairs = &digits[1..];
        }
        for pair in pairs.chunks(2) {
            out.push((hex(pair[0])? << 4) | hex(pair[1])?);
        }
        return Ok(out);
    }
    if let Some(c) = input.iter().find(|c| !matches!(c, b'0' | b'1')) {
        return Err(invalid_character(*c));
    }
    Ok(bits_to_bit(input.iter().map(|c| *c == b'1')))
}

/// `Bit::BlobToBit`.
pub fn blob_to_bit(blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(blob.len() + 1);
    out.push(0);
    out.extend_from_slice(blob);
    out
}

/// `Bit::GetFirstByte`.
fn first_byte(bits: &[u8]) -> u8 {
    (u16::from(bits[1]) & ((1u16 << (8 - padding(bits))) - 1)) as u8
}

/// `Bit::BitToBlob`: the bytes of the bits, without the padding.
pub fn bit_to_bytes(bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bits.len() - 1);
    out.push(first_byte(bits));
    out.extend_from_slice(&bits[2..]);
    out
}

/// `CreateBitStringSortKey`: a byte per bit, above the BLOB escape range.
pub fn byte_comparable(bits: &[u8]) -> Vec<u8> {
    (0..bit_length(bits))
        .map(|i| if get_bit(bits, i) { 3 } else { 2 })
        .collect()
}

/// `Bit::BitCount`.
pub fn bit_count(bits: &[u8]) -> usize {
    bits[1..].iter().map(|b| b.count_ones() as usize).sum::<usize>() - padding(bits)
}

/// `Bit::BitPosition`: the 1-based position of `substring`, or 0.
pub fn bit_position(substring: &[u8], bits: &[u8]) -> usize {
    let (length, sub_length) = (bit_length(bits), bit_length(substring));
    if sub_length > length {
        return 0;
    }
    (0..=length - sub_length)
        .find(|&start| (0..sub_length).all(|i| get_bit(bits, start + i) == get_bit(substring, i)))
        .map_or(0, |start| start + 1)
}

/// `Bit::BitwiseAnd`, `BitwiseOr` and `BitwiseXor`, bytewise; the padding stays 1.
pub fn bitwise(left: &[u8], right: &[u8], op: impl Fn(u8, u8) -> u8) -> Vec<u8> {
    let (length, combine) = (bit_length(left), |i| {
        op(u8::from(get_bit(left, i)), u8::from(get_bit(right, i))) != 0
    });
    bits_to_bit((0..length).map(combine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_round_trip() {
        for text in ["0", "1", "0101", "00000000", "10101010101", "111100001111000011"] {
            let bits = string_to_bit(text.as_bytes()).unwrap();
            assert_eq!(bit_to_string(&bits), text);
            assert_eq!(bit_length(&bits), text.len());
        }
        assert_eq!(bit_to_string(&string_to_bit(b"xFA").unwrap()), "11111010");
        assert_eq!(bit_to_string(&string_to_bit(b"xA").unwrap()), "1010");
        assert!(string_to_bit(b"x").is_err());
        assert!(string_to_bit(b"012").is_err());
        assert_eq!(bit_count(&string_to_bit(b"0101").unwrap()), 2);
        assert_eq!(
            bit_position(&string_to_bit(b"010").unwrap(), &string_to_bit(b"1110101").unwrap()),
            4
        );
        assert_eq!(bit_to_bytes(&string_to_bit(b"00000001").unwrap()), [1]);
        assert_eq!(bit_to_bytes(&string_to_bit(b"101").unwrap()), [5]);
    }
}
