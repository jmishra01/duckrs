// Ported from duckdb/src/common/types/bignum.cpp and duckdb/src/function/cast/bignum_casts.cpp
// @ rust-port-baseline
//
//! BIGNUM values: a 3-byte header with the sign and the number of data bytes, then the magnitude,
//! most significant byte first. All of a negative number's bytes are inverted, so that the bytes
//! of two BIGNUMs compare like the numbers.

const HEADER_SIZE: usize = 3;
const DECIMAL_BASE: u64 = 1_000_000_000;
const DECIMAL_SHIFT: usize = 9;

/// A BIGNUM from a magnitude without leading zero bytes, which is `[0]` for zero.
pub fn from_magnitude(magnitude: &[u8], negative: bool) -> Vec<u8> {
    let mut header = magnitude.len() as u32 | 0x0080_0000;
    if negative {
        header = !header;
    }
    let mut out = Vec::with_capacity(HEADER_SIZE + magnitude.len());
    out.extend_from_slice(&header.to_be_bytes()[1..]);
    out.extend(magnitude.iter().map(|b| if negative { !b } else { *b }));
    out
}

pub fn is_negative(bignum: &[u8]) -> bool {
    bignum[0] & 0x80 == 0
}

/// The magnitude, most significant byte first.
pub fn magnitude(bignum: &[u8]) -> Vec<u8> {
    let negative = is_negative(bignum);
    bignum[HEADER_SIZE..]
        .iter()
        .map(|b| if negative { !b } else { *b })
        .collect()
}

fn trimmed(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len() - 1);
    &bytes[start..]
}

/// `IntToBignum` and `HugeintCastToBignum`.
pub fn from_i128(value: i128) -> Vec<u8> {
    from_magnitude(trimmed(&value.unsigned_abs().to_be_bytes()), value < 0)
}

pub fn from_u128(value: u128) -> Vec<u8> {
    from_magnitude(trimmed(&value.to_be_bytes()), false)
}

/// `DoubleToBignum`: `None` for what is not finite.
pub fn from_f64(value: f64) -> Option<Vec<u8>> {
    if !value.is_finite() {
        return None;
    }
    // round half to even like the casts to BIGINT and HUGEINT: the byte loop is only exact for
    // whole numbers
    let mut rest = value.round_ties_even().abs();
    if rest == 0.0 {
        return Some(from_i128(0));
    }
    let mut bytes = Vec::new();
    while rest > 0.0 {
        let truncated = (rest / 256.0).floor();
        bytes.push((rest - truncated * 256.0) as u8);
        rest = truncated;
    }
    bytes.reverse();
    Some(from_magnitude(&bytes, value < 0.0))
}

/// `Bignum::VarcharFormatting` + `EncodeVarcharBignum`: fractions round half up, in magnitude.
pub fn from_str(text: &[u8]) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }
    let negative = text[0] == b'-';
    let mut start = usize::from(negative || text[0] == b'+');
    let mut at_least_one_zero = false;
    while start < text.len() && text[start] == b'0' {
        start += 1;
        at_least_one_zero = true;
    }
    if start == text.len() {
        return at_least_one_zero.then(|| from_i128(0));
    }
    let integer_end = text[start..]
        .iter()
        .position(|c| !c.is_ascii_digit())
        .map_or(text.len(), |p| start + p);
    let mut round_up = false;
    if integer_end < text.len() {
        if text[integer_end] != b'.' {
            return None;
        }
        let fraction = &text[integer_end + 1..];
        if !fraction.iter().all(u8::is_ascii_digit) {
            return None;
        }
        round_up = fraction.first().is_some_and(|d| *d >= b'5');
        if integer_end == start && !at_least_one_zero && fraction.is_empty() {
            return None;
        }
    }
    let mut digits = text[start..integer_end].to_vec();
    if round_up {
        let mut position = digits.len();
        loop {
            if position == 0 {
                digits.insert(0, b'1');
                break;
            }
            position -= 1;
            if digits[position] == b'9' {
                digits[position] = b'0';
            } else {
                digits[position] += 1;
                break;
            }
        }
    }
    if digits.is_empty() {
        return Some(from_i128(0));
    }
    Some(from_decimal_digits(&digits, negative))
}

/// `Bignum::EncodeBignum`.
fn from_decimal_digits(digits: &[u8], negative: bool) -> Vec<u8> {
    let mut limbs: Vec<u32> = Vec::with_capacity(digits.len() / DECIMAL_SHIFT + 1);
    let first = match digits.len() % DECIMAL_SHIFT {
        0 => DECIMAL_SHIFT,
        size => size,
    };
    let mut position = 0;
    let mut size = first;
    while position < digits.len() {
        let mut carry = digits[position..position + size]
            .iter()
            .fold(0u64, |acc, d| acc * 10 + u64::from(d - b'0'));
        let base = 10u64.pow(size as u32);
        for limb in &mut limbs {
            let value = u64::from(*limb) * base + carry;
            *limb = value as u32;
            carry = value >> 32;
        }
        if carry != 0 {
            limbs.push(carry as u32);
        }
        position += size;
        size = DECIMAL_SHIFT;
    }
    let bytes: Vec<u8> = limbs.iter().rev().flat_map(|l| l.to_be_bytes()).collect();
    from_magnitude(trimmed(&bytes), negative)
}

/// `Bignum::BignumToVarchar`.
pub fn to_string(bignum: &[u8]) -> String {
    let bytes = magnitude(bignum);
    let mut limbs: Vec<u64> = Vec::new();
    for chunk in bytes.rchunks(4).rev() {
        let mut high = chunk.iter().fold(0u64, |acc, b| acc << 8 | u64::from(*b));
        for limb in &mut limbs {
            let value = *limb << (8 * chunk.len()) | high;
            high = value / DECIMAL_BASE;
            *limb = value % DECIMAL_BASE;
        }
        while high != 0 {
            limbs.push(high % DECIMAL_BASE);
            high /= DECIMAL_BASE;
        }
    }
    let Some(top) = limbs.pop() else {
        return "0".to_string();
    };
    let mut out = String::with_capacity(limbs.len() * DECIMAL_SHIFT + 11);
    if is_negative(bignum) {
        out.push('-');
    }
    out.push_str(&top.to_string());
    for limb in limbs.iter().rev() {
        out.push_str(&format!("{limb:09}"));
    }
    out
}

/// `Bignum::BignumToDouble`: `None` if the value does not fit a double.
pub fn to_f64(bignum: &[u8]) -> Option<f64> {
    let mut result = 0.0;
    for (position, byte) in magnitude(bignum).iter().rev().enumerate() {
        result += f64::from(*byte) * 256f64.powi(position as i32);
    }
    if is_negative(bignum) {
        result = -result;
    }
    result.is_finite().then_some(result)
}

/// The value's sign and magnitude, if the magnitude fits 128 bits.
pub fn to_u128_magnitude(bignum: &[u8]) -> Option<(bool, u128)> {
    let bytes = magnitude(bignum);
    let bytes = trimmed(&bytes);
    (bytes.len() <= 16).then(|| {
        let value = bytes.iter().fold(0u128, |acc, b| acc << 8 | u128::from(*b));
        (is_negative(bignum), value)
    })
}

/// The most data bytes a BIGNUM can hold (`Bignum::MAX_DATA_SIZE`).
pub const MAX_DATA_SIZE: usize = 8_388_607;

/// A magnitude in base 2^32, least significant limb first, without leading zero limbs.
type Limbs = Vec<u32>;

fn to_limbs(bignum: &[u8]) -> Limbs {
    let bytes = magnitude(bignum);
    let mut limbs: Limbs = bytes
        .rchunks(4)
        .map(|chunk| chunk.iter().fold(0u32, |acc, b| acc << 8 | u32::from(*b)))
        .collect();
    trim(&mut limbs);
    limbs
}

fn trim(limbs: &mut Limbs) {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
}

/// `ProduceOverUnderFlowError`, for a result of `size` data bytes.
fn overflow_error(negative: bool, size: usize) -> String {
    format!(
        "{} in Bignum Operation. A Bignum can hold max {MAX_DATA_SIZE} data bytes. Current bignum has {size} bytes.",
        if negative { "Underflow" } else { "Overflow" }
    )
}

fn from_limbs(limbs: &[u32], negative: bool) -> Result<Vec<u8>, String> {
    let bytes: Vec<u8> = limbs.iter().rev().flat_map(|l| l.to_be_bytes()).collect();
    if bytes.is_empty() {
        return Ok(from_i128(0));
    }
    let bytes = trimmed(&bytes);
    if bytes.len() > MAX_DATA_SIZE {
        return Err(overflow_error(negative, bytes.len()));
    }
    Ok(from_magnitude(bytes, negative))
}

fn compare_limbs(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_limbs(a: &[u32], b: &[u32]) -> Limbs {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for (i, limb) in long.iter().enumerate() {
        let sum = u64::from(*limb) + u64::from(short.get(i).copied().unwrap_or(0)) + carry;
        out.push(sum as u32);
        carry = sum >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b`, where `a >= b`.
fn subtract_limbs(a: &[u32], b: &[u32]) -> Limbs {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, limb) in a.iter().enumerate() {
        let difference = i64::from(*limb) - i64::from(b.get(i).copied().unwrap_or(0)) - borrow;
        borrow = i64::from(difference < 0);
        out.push(difference as u32);
    }
    trim(&mut out);
    out
}

fn signed_add(a: &[u32], a_negative: bool, b: &[u32], b_negative: bool) -> Result<Vec<u8>, String> {
    if a_negative == b_negative {
        return from_limbs(&add_limbs(a, b), a_negative);
    }
    if compare_limbs(a, b).is_lt() {
        from_limbs(&subtract_limbs(b, a), b_negative)
    } else {
        from_limbs(&subtract_limbs(a, b), a_negative)
    }
}

/// `BignumIntermediate::Add`.
pub fn add(left: &[u8], right: &[u8]) -> Result<Vec<u8>, String> {
    signed_add(&to_limbs(left), is_negative(left), &to_limbs(right), is_negative(right))
}

/// `left - right`.
pub fn subtract(left: &[u8], right: &[u8]) -> Result<Vec<u8>, String> {
    signed_add(
        &to_limbs(left),
        is_negative(left),
        &to_limbs(right),
        !is_negative(right),
    )
}

/// `BignumIntermediate::Negate`.
pub fn negate(value: &[u8]) -> Vec<u8> {
    let limbs = to_limbs(value);
    from_limbs(&limbs, !is_negative(value)).expect("the size does not change")
}

/// `BignumIntermediate::Multiply`.
pub fn multiply(left: &[u8], right: &[u8]) -> Result<Vec<u8>, String> {
    let (a, b) = (to_limbs(left), to_limbs(right));
    let negative = is_negative(left) != is_negative(right);
    if a.is_empty() || b.is_empty() {
        return Ok(from_i128(0));
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, y) in b.iter().enumerate() {
            let value = u64::from(*x) * u64::from(*y) + u64::from(out[i + j]) + carry;
            out[i + j] = value as u32;
            carry = value >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    trim(&mut out);
    from_limbs(&out, negative)
}

/// `numerator / denominator`, Knuth's algorithm D: the quotient and the remainder.
fn divide_limbs(numerator: &[u32], denominator: &[u32]) -> (Limbs, Limbs) {
    if compare_limbs(numerator, denominator).is_lt() {
        return (Vec::new(), numerator.to_vec());
    }
    if let [divisor] = denominator {
        let divisor = u64::from(*divisor);
        let mut quotient = vec![0u32; numerator.len()];
        let mut remainder = 0u64;
        for i in (0..numerator.len()).rev() {
            let value = remainder << 32 | u64::from(numerator[i]);
            quotient[i] = (value / divisor) as u32;
            remainder = value % divisor;
        }
        trim(&mut quotient);
        let mut rest = vec![remainder as u32];
        trim(&mut rest);
        return (quotient, rest);
    }
    let shift = denominator[denominator.len() - 1].leading_zeros();
    let shifted = |limbs: &[u32], extra: usize| {
        let mut out = vec![0u32; limbs.len() + extra];
        for (i, limb) in limbs.iter().enumerate() {
            let value = u64::from(*limb) << shift;
            out[i] |= value as u32;
            if i + 1 < out.len() {
                out[i + 1] = (value >> 32) as u32;
            }
        }
        out
    };
    let d = shifted(denominator, 0);
    let mut n = shifted(numerator, 1);
    let size = d.len();
    let base = 1u64 << 32;
    let mut quotient = vec![0u32; numerator.len() - size + 1];
    for j in (0..quotient.len()).rev() {
        let top = u64::from(n[j + size]) << 32 | u64::from(n[j + size - 1]);
        let mut estimate = top / u64::from(d[size - 1]);
        let mut rest = top % u64::from(d[size - 1]);
        while estimate >= base || estimate * u64::from(d[size - 2]) > (rest << 32 | u64::from(n[j + size - 2])) {
            estimate -= 1;
            rest += u64::from(d[size - 1]);
            if rest >= base {
                break;
            }
        }
        let mut borrow = 0i64;
        let mut carry = 0u64;
        for i in 0..size {
            let product = estimate * u64::from(d[i]) + carry;
            carry = product >> 32;
            let value = i64::from(n[i + j]) - borrow - (product & 0xFFFF_FFFF) as i64;
            n[i + j] = value as u32;
            borrow = i64::from(value < 0);
        }
        let value = i64::from(n[j + size]) - borrow - carry as i64;
        n[j + size] = value as u32;
        if value < 0 {
            estimate -= 1;
            let mut carry = 0u64;
            for i in 0..size {
                let sum = u64::from(n[i + j]) + u64::from(d[i]) + carry;
                n[i + j] = sum as u32;
                carry = sum >> 32;
            }
            n[j + size] = n[j + size].wrapping_add(carry as u32);
        }
        quotient[j] = estimate as u32;
    }
    trim(&mut quotient);
    let mut remainder = vec![0u32; size];
    for i in 0..size {
        let high = if shift == 0 {
            0
        } else {
            u64::from(n[i + 1]) << (32 - shift)
        };
        remainder[i] = ((u64::from(n[i]) >> shift) | high) as u32;
    }
    trim(&mut remainder);
    (quotient, remainder)
}

/// `BignumIntermediate::TryDivMod`: the truncated quotient and the remainder, which takes the
/// sign of the dividend, or `None` for a division by zero.
pub fn div_mod(left: &[u8], right: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (a, b) = (to_limbs(left), to_limbs(right));
    if b.is_empty() {
        return None;
    }
    let (quotient, remainder) = divide_limbs(&a, &b);
    let quotient_negative = is_negative(left) != is_negative(right);
    let quotient = from_limbs(&quotient, quotient_negative && !quotient.is_empty());
    let remainder = from_limbs(&remainder, is_negative(left) && !remainder.is_empty());
    Some((quotient.expect("not larger"), remainder.expect("not larger")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(text: &str) -> Vec<u8> {
        from_str(text.as_bytes()).unwrap()
    }

    #[test]
    fn arithmetic_is_exact() {
        let text = |r: Result<Vec<u8>, String>| to_string(&r.unwrap());
        assert_eq!(text(add(&big("999999999"), &big("1"))), "1000000000");
        assert_eq!(
            text(add(&big("18446744073709551615"), &big("1"))),
            "18446744073709551616"
        );
        assert_eq!(text(add(&big("5"), &big("-7"))), "-2");
        assert_eq!(text(add(&big("7"), &big("-7"))), "0");
        assert_eq!(text(subtract(&big("-5"), &big("-5"))), "0");
        assert_eq!(text(subtract(&big("0"), &big("3"))), "-3");
        assert_eq!(to_string(&negate(&big("0"))), "0");
        let product = "1219326311370217952237463801111263526900";
        assert_eq!(
            text(multiply(&big("12345678901234567890"), &big("98765432109876543210"))),
            product
        );
        assert_eq!(text(multiply(&big("-3"), &big("4"))), "-12");
        let (q, r) = div_mod(&big(product), &big("98765432109876543210")).unwrap();
        assert_eq!(
            (to_string(&q), to_string(&r)),
            ("12345678901234567890".into(), "0".into())
        );
        let (q, r) = div_mod(&big("-7"), &big("2")).unwrap();
        assert_eq!((to_string(&q), to_string(&r)), ("-3".into(), "-1".into()));
        let (q, r) = div_mod(&big("7"), &big("-2")).unwrap();
        assert_eq!((to_string(&q), to_string(&r)), ("-3".into(), "1".into()));
        let (q, r) = div_mod(&big("3"), &big("-7")).unwrap();
        assert_eq!((to_string(&q), to_string(&r)), ("0".into(), "3".into()));
        assert_eq!(div_mod(&big("1"), &big("0")), None);
        // checked against the multiplication, with divisors of several limbs
        let (a, b) = (
            big("340282366920938463463374607431768211457"),
            big("18446744073709551629"),
        );
        let (q, r) = div_mod(&a, &b).unwrap();
        assert_eq!(add(&multiply(&q, &b).unwrap(), &r).unwrap(), a);
    }

    #[test]
    fn numbers_round_trip() {
        for text in [
            "0",
            "1",
            "-1",
            "255",
            "256",
            "-65536",
            "4294967296",
            "1000000000000000000000",
            "-340282366920938463463374607431768211456",
            "179769313486231570814527423731704356798070567525844996598917476803157260780028538760589558632766878",
        ] {
            assert_eq!(to_string(&from_str(text.as_bytes()).unwrap()), text);
        }
        assert_eq!(from_str(b"-0"), Some(from_i128(0)));
        assert_eq!(to_string(&from_str(b"0.5").unwrap()), "1");
        assert_eq!(to_string(&from_str(b"-1.5").unwrap()), "-2");
        assert_eq!(to_string(&from_str(b"99.9").unwrap()), "100");
        assert_eq!(from_str(b"1.2.3"), None);
        assert_eq!(from_str(b"-"), None);
        assert_eq!(from_str(b"+-0"), None);
        assert_eq!(to_string(&from_i128(i128::MIN)), i128::MIN.to_string());
        assert_eq!(to_string(&from_u128(u128::MAX)), u128::MAX.to_string());
    }

    #[test]
    fn bytes_compare_like_numbers() {
        let mut values: Vec<i128> = vec![-70000, -256, -255, -1, 0, 1, 255, 256, 70000];
        let encoded: Vec<Vec<u8>> = values.iter().map(|v| from_i128(*v)).collect();
        let mut sorted = encoded.clone();
        sorted.sort();
        assert_eq!(encoded, sorted);
        values.clear();
    }
}
