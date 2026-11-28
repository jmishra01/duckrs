// Ported from duckdb/src/common/operator/string_cast.cpp, cast_helpers.hpp (DecimalToString) and
// types/blob.cpp (Blob::ToString) @ rust-port-baseline

use crate::fmt_float::format_finite;
pub use crate::fmt_float::{FloatFormat, FloatSpec, format_hex_float, format_with_spec};

/// Formats a DOUBLE like `duckdb_fmt::format("{}", value)`: fixed notation when the decimal exponent
/// is in `-4..16` (with `.0` for integral values), otherwise scientific notation with a signed,
/// at-least-two-digit exponent. The digits are what fmt's Grisu produces, which are nearly always,
/// but not always, the shortest and closest ones (see `fmt_float`).
pub fn format_double(value: f64) -> String {
    if value.is_finite() {
        format_finite(value, false)
    } else {
        format_non_finite(value.is_nan(), value.is_sign_negative())
    }
}

/// Formats a FLOAT in the same way. fmt looks for digits that round-trip as an `f32`, but where
/// that fails it prints the shortest ones of the `f64` that the value converts to.
pub fn format_float(value: f32) -> String {
    if value.is_finite() {
        format_finite(f64::from(value), true)
    } else {
        format_non_finite(value.is_nan(), value.is_sign_negative())
    }
}

fn format_non_finite(nan: bool, negative: bool) -> String {
    match (nan, negative) {
        (true, false) => "nan",
        (true, true) => "-nan",
        (false, false) => "inf",
        (false, true) => "-inf",
    }
    .into()
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

/// A string inside of a nested value's rendering (`VectorCastHelpers::WriteEscapedString`): quoted
/// with `'` where it would not read back as itself. STRUCT keys are always quoted.
pub fn escape_nested_string(text: &str, struct_key: bool) -> String {
    let bytes = text.as_bytes();
    let is_special = |c: &u8| {
        matches!(
            c,
            b'"' | b'\'' | b'(' | b')' | b',' | b':' | b'=' | b'[' | b'\\' | b']' | b'{' | b'}'
        )
    };
    let needs_quotes = bytes.is_empty()
        || struct_key
        || crate::string_util::is_space(bytes[0])
        || (bytes.len() >= 2 && crate::string_util::is_space(bytes[bytes.len() - 1]))
        || text.eq_ignore_ascii_case("null")
        || bytes.iter().any(is_special);
    if !needs_quotes {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        if c == '\'' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
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
            (8.124038696289062, "8.124038696289062"),
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
            (1e30, "1e+30"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (f32::INFINITY, "inf"),
            (f32::NEG_INFINITY, "-inf"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_float(*value), *expected, "{value:e}");
        }
    }

    /// Bit patterns, and what DuckDB v1.5.6 prints for them: awkward cases of fmt's digit generation.
    #[test]
    fn awkward_floats_match_duckdb() {
        let cases: &[(u32, &str)] = &[
            // the examples
            (0x427ff800, "63.9921875"),
            (0x3dcccccd, "0.1"),
            (0x7149f2ca, "1e+30"),
            // prints the shortest DOUBLE, as Grisu gives up on these
            (0x39800000, "0.000244140625"),
            (0x3ac00000, "0.00146484375"),
            (0x40004000, "2.00390625"),
            (0x4002c000, "2.04296875"),
            (0x500001c6, "8590399488.0"),
            (0x50000438, "8591040512.0"),
            // subnormals, and around the smallest normal value
            (0x00000001, "1e-45"),
            (0x00000002, "3e-45"),
            (0x00000003, "4e-45"),
            (0x000fffff, "1.469367e-39"),
            (0x007fffff, "1.1754942e-38"),
            (0x00800000, "1.1754944e-38"),
            (0x00800001, "1.1754945e-38"),
            // powers of two
            (0x3f800000, "1.0"),
            (0x40000000, "2.0"),
            (0x01000000, "2.3509887e-38"),
            (0x7f000000, "1.7014118e+38"),
            (0x7f7fffff, "3.4028235e+38"),
            // fixed and scientific
            (0x38d1b717, "0.0001"),
            (0x38d1b68e, "9.9999e-05"),
            (0x3727c5ac, "1e-05"),
            (0x58635fa9, "1000000000000000.0"),
            (0x5a0e1bc9, "9999999000000000.0"),
            (0x5a0e1bca, "1e+16"),
            (0x5a0e1bcb, "1.0000001e+16"),
        ];
        for (bits, expected) in cases {
            let value = f32::from_bits(*bits);
            assert_eq!(format_float(value), *expected, "{bits:#x}");
            assert_eq!(format_float(-value), format!("-{expected}"), "{bits:#x}");
        }
    }

    /// The same for doubles.
    #[test]
    fn awkward_doubles_match_duckdb() {
        let cases: &[(u64, &str)] = &[
            // ties between two shortest forms
            (0x40203f8200000000, "8.124038696289062"),
            (0x3e60000000000000, "2.9802322387695312e-08"),
            (0x42a0000000000020, "8796093022208.062"),
            (0x42bfffffffffffd0, "35184372088831.812"),
            (0x42d0000000000008, "70368744177664.12"),
            (0x42effffffffffff4, "281474976710655.62"),
            // subnormals, and around the smallest normal value
            (0x0000000000000002, "1e-323"),
            (0x0000000000000003, "1.5e-323"),
            (0x000fffffffffffff, "2.225073858507201e-308"),
            (0x0010000000000000, "2.2250738585072014e-308"),
            (0x0010000000000001, "2.225073858507202e-308"),
            // fixed and scientific
            (0x3f1a36e2eb1c432d, "0.0001"),
            (0x3f1a36e2eb1c432c, "9.999999999999999e-05"),
            (0x3ee4f8b588e368f1, "1e-05"),
            (0x4341c37937e07fff, "9999999999999998.0"),
            (0x4341c37937e08000, "1e+16"),
            (0x4341c37937e08001, "1.0000000000000002e+16"),
            // the powers of two from 2^53 up, which fmt's fallback scales by the bits it shifted
            (0x4340000000000000, "9007199254740992.0"),
            (0x4500000000000000, "2.4178516392292583e+24"),
            (0x45a0000000000000, "2.4758800785707605e+27"),
            (0x7260000000000000, "8.535058474086213e+242"),
        ];
        for (bits, expected) in cases {
            let value = f64::from_bits(*bits);
            assert_eq!(format_double(value), *expected, "{bits:#x}");
            assert_eq!(format_double(-value), format!("-{expected}"), "{bits:#x}");
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
