// Ported from duckdb/src/common/operator/string_cast.cpp, cast_helpers.hpp (DecimalToString) and
// types/blob.cpp (Blob::ToString) @ rust-port-baseline

/// Formats a DOUBLE like `duckdb_fmt::format("{}", value)`: the shortest digits that round-trip,
/// in fixed notation when the decimal exponent is in `-4..16` (with `.0` for integral values),
/// otherwise in scientific notation with a signed, at-least-two-digit exponent.
pub fn format_double(value: f64) -> String {
    if value.is_nan() {
        return if value.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.into();
    }
    format_shortest(&format!("{value:e}"))
}

/// Formats a FLOAT with the shortest digits that round-trip as an `f32`.
pub fn format_float(value: f32) -> String {
    if value.is_nan() {
        return if value.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.into();
    }
    format_shortest(&format!("{value:e}"))
}

/// Re-renders Rust's shortest `{:e}` output (`-1.2345e-7`) in fmt's general format.
fn format_shortest(scientific: &str) -> String {
    let (mantissa, exponent) = scientific.split_once('e').expect("{:e} output has an exponent");
    let exponent: i32 = exponent.parse().expect("valid exponent");
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut out = String::with_capacity(digits.len() + 8);
    if negative {
        out.push('-');
    }
    if (-4..16).contains(&exponent) {
        if exponent < 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exponent - 1) as usize));
            out.push_str(&digits);
        } else {
            let int_len = exponent as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exponent.unsigned_abs()));
    }
    out
}

/// Formats an unscaled decimal value (`DecimalToString::FormatDecimal`). When `width == scale`
/// there is no integer part, so `0.5` in `DECIMAL(1,1)` prints as `.5`.
pub fn format_decimal(value: i128, width: u8, scale: u8) -> String {
    let negative = value < 0;
    let magnitude = value.unsigned_abs();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&magnitude.to_string());
        return out;
    }
    let power = 10u128.pow(u32::from(scale));
    let (major, minor) = (magnitude / power, magnitude % power);
    if width > scale {
        out.push_str(&major.to_string());
    }
    out.push('.');
    out.push_str(&format!("{minor:0width$}", width = usize::from(scale)));
    out
}

/// Renders a BLOB (`Blob::ToString`): printable ASCII as-is, everything else (including `\`,
/// `'` and `"`) as `\xHH`.
pub fn format_blob(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        let regular = (32..=126).contains(&b) && b != b'\\' && b != b'\'' && b != b'"';
        if regular {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected strings come from `SELECT x::VARCHAR` in DuckDB.
    #[test]
    fn doubles_match_duckdb() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (100.0, "100.0"),
            (0.1, "0.1"),
            (1.5e-7, "1.5e-07"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123456789012345.0, "123456789012345.0"),
            (1234567890123456.0, "1234567890123456.0"),
            (12345678901234567.0, "1.2345678901234568e+16"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1e20, "1e+20"),
            (f64::MAX, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (f64::NAN, "nan"),
            (-f64::NAN, "-nan"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (1.0 / 3.0, "0.3333333333333333"),
            (-12.75, "-12.75"),
            (9.999e-5, "9.999e-05"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_double(*value), *expected, "{value:e}");
        }
    }

    #[test]
    // 3.14159 is a captured DuckDB test vector, not an approximation of PI.
    #[allow(clippy::approx_constant)]
    fn floats_match_duckdb() {
        let cases: &[(f32, &str)] = &[
            (0.1, "0.1"),
            (1e10, "10000000000.0"),
            (3.14159, "3.14159"),
            (1e-7, "1e-07"),
            (16777216.0, "16777216.0"),
            (1e16, "1e+16"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_float(*value), *expected, "{value:e}");
        }
    }

    #[test]
    fn decimals_match_duckdb() {
        assert_eq!(format_decimal(12345, 10, 2), "123.45");
        assert_eq!(format_decimal(-5, 4, 2), "-0.05");
        assert_eq!(format_decimal(5, 3, 1), "0.5");
        assert_eq!(format_decimal(-5, 3, 1), "-0.5");
        assert_eq!(format_decimal(5, 3, 0), "5");
        assert_eq!(format_decimal(0, 5, 3), "0.000");
        assert_eq!(
            format_decimal(-12345678901234567890123456789, 38, 9),
            "-12345678901234567890.123456789"
        );
        assert_eq!(format_decimal(100000000, 18, 18), ".000000000100000000");
    }

    #[test]
    fn blobs_match_duckdb() {
        assert_eq!(format_blob(b"\x00ab\xFF"), "\\x00ab\\xFF");
        assert_eq!(format_blob(b"abc"), "abc");
        assert_eq!(format_blob(b"a\\b'c\"d"), "a\\x5Cb\\x27c\\x22d");
        assert_eq!(format_blob(b"\xAB\x0A"), "\\xAB\\x0A");
    }
}
