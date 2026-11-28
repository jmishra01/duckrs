//! `format` and `printf` as fmt writes them. A FLOAT or a DECIMAL argument is taken as a DOUBLE, so
//! a FLOAT prints digits that a cast to VARCHAR does not. The expected text comes from DuckDB v1.5.6.

use pintail::Database;

fn text(sql: &str) -> String {
    let mut connection = Database::open_in_memory().unwrap().connect().unwrap();
    let result = connection.query(sql).unwrap();
    result.collection.row(0)[0].try_to_string().unwrap()
}

#[test]
fn a_float_argument_prints_as_the_double_it_widens_to() {
    for (expression, expected) in [
        ("format('{}', 0.1::FLOAT)", "0.10000000149011612"),
        ("format('{}', 1e30::FLOAT)", "1.0000000150474662e+30"),
        ("format('{}', 63.9921875::FLOAT)", "63.9921875"),
        (
            "format('{0} {0}', 0.3::FLOAT)",
            "0.30000001192092896 0.30000001192092896",
        ),
        ("format('{:>25}', 0.1::FLOAT)", "      0.10000000149011612"),
        ("format('{:.20f}', 0.1::FLOAT)", "0.10000000149011611938"),
        ("printf('%.20f', 0.1::FLOAT)", "0.10000000149011611938"),
        ("printf('%f %.1f', 0.1::FLOAT, 2.5::FLOAT)", "0.100000 2.5"),
    ] {
        assert_eq!(text(&format!("SELECT {expression}")), expected, "{expression}");
    }
}

#[test]
fn a_float_inside_a_nested_argument_keeps_its_own_digits() {
    assert_eq!(text("SELECT format('{}', [0.1::FLOAT])"), "[0.1]");
}

#[test]
fn a_float_parameter_widens_as_well() {
    let mut connection = Database::open_in_memory().unwrap().connect().unwrap();
    connection.query("PREPARE p AS SELECT format('{}', ?)").unwrap();
    let result = connection.query("EXECUTE p(0.1::FLOAT)").unwrap();
    assert_eq!(
        result.collection.row(0)[0].try_to_string().unwrap(),
        "0.10000000149011612"
    );
}

#[test]
fn a_double_follows_its_format_specifier() {
    for (expression, expected) in [
        ("format('{:.3}', 63.9921875::DOUBLE)", "64.0"),
        ("format('{:.3}', 0::DOUBLE)", "0.00"),
        ("format('{:.0}', 2.5::DOUBLE)", "2.5"),
        ("format('{:.3g}', 0::DOUBLE)", "0"),
        ("printf('%g', 100000::DOUBLE)", "100000"),
        ("printf('%g', 1000000::DOUBLE)", "1e+06"),
        ("printf('%#g', 1.5::DOUBLE)", "1.50000"),
        ("printf('%.0f', 2.5::DOUBLE)", "2"),
        ("printf('%#.0f', 2.5::DOUBLE)", "2.."),
        ("printf('%#.0f', 5e-324::DOUBLE)", "0"),
        ("printf('%.2f', 0.001::DOUBLE)", "0.00"),
        ("printf('%e', 0::DOUBLE)", "0.000000e+00"),
        ("printf('%E', 1234.5::DOUBLE)", "1.234500E+03"),
        ("printf('%.25f', 0.1::DOUBLE)", "0.1000000000000000055511151"),
        ("format('{:.0f}', 1e30::DOUBLE)", "1000000000000000019884624838656"),
        (
            "format('{:.1f}', 1e300::DOUBLE)",
            "1000000000000000052532413974404335021972656250000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000.0",
        ),
        ("printf('%-012.3f|', 1.5::DOUBLE)", "00000001.500|"),
        ("printf('%+08.2f', 1.5::DOUBLE)", "+0001.50"),
        ("printf('% .2f', 1.5::DOUBLE)", " 1.50"),
        ("format('{:010.3f}', -1.5::DOUBLE)", "-00001.500"),
        ("format('{:>10}|', 1.5::DOUBLE)", "       1.5|"),
        ("printf('%e', 'nan'::DOUBLE)", "nan"),
        ("printf('%E', 'nan'::DOUBLE)", "NAN"),
        ("printf('%g', '-inf'::DOUBLE)", "-inf"),
        ("printf('%G', 'inf'::DOUBLE)", "INF"),
        ("printf('%f', 'nan'::DOUBLE)", "nan"),
        ("printf('%+f', 'nan'::DOUBLE)", "+nan"),
        ("printf('%10f|', 'nan'::DOUBLE)", "       nan|"),
        ("printf('%-10f|', 'nan'::DOUBLE)", "nan       |"),
        ("printf('%010f|', 'nan'::DOUBLE)", "nan0000000|"),
        ("format('{:10}|', 'nan'::DOUBLE)", "nan       |"),
        ("format('{:>10}|', 'nan'::DOUBLE)", "       nan|"),
        ("format('{:010.5f}|', 'inf'::DOUBLE)", "inf0000000|"),
        ("format('{}', 1.10::DECIMAL(3,2))", "1.1"),
        ("printf('%.4f', 0.5)", "0.5000"),
        ("printf('%.2f', 1.5::DECIMAL(3,1))", "1.50"),
    ] {
        assert_eq!(text(&format!("SELECT {expression}")), expected, "{expression}");
    }
}

#[test]
fn the_rest_of_the_specifier_syntax_works() {
    for (expression, expected) in [
        ("format('{:*^10}|', 1.5::DOUBLE)", "***1.5****|"),
        ("format('{:*>10}|', 1.5::DOUBLE)", "*******1.5|"),
        ("format('{:*=+10.2f}|', 1.5::DOUBLE)", "+*****1.50|"),
        ("format('{:+.2f}', 1.5::DOUBLE)", "+1.50"),
        ("format('{:,}', 1234567.5::DOUBLE)", "1,234,567.5"),
        ("format('{:t.}', 1234567.5::DOUBLE)", "1.234.567,5"),
        ("format('{:,}', 1234567)", "1,234,567"),
        ("format('{:_}', 1234567)", "1_234_567"),
        ("printf('%,.2f', 1234567.5::DOUBLE)", "1,234,567.50"),
        ("printf('%,d', 1234567)", "1,234,567"),
        ("printf('%.d', 1234567)", "1.234.567"),
        ("printf('%a', 1.5::DOUBLE)", "0x1.8p+0"),
        ("format('{:A}', 1.5::DOUBLE)", "0X1.8P+0"),
        ("printf('%.2a', 1.999::DOUBLE)", "0x2.00p+0"),
        ("printf('%a', 0::DOUBLE)", "0x0p+0"),
        ("printf('%.*f', 2, 1.5::DOUBLE)", "1.50"),
        ("printf('%*d|', 5, 1)", "    1|"),
        ("printf('%*d|', -5, 1)", "1    |"),
        ("format('{:{}}|', 1.5::DOUBLE, 8)", "     1.5|"),
        ("format('{:.{}f}', 1.5::DOUBLE, 3)", "1.500"),
        ("format('{0:{1}}|', 1, 4)", "   1|"),
        ("printf('%2$d %1$d', 1, 2)", "2 1"),
        ("format('{:5}|', 'x')", "x    |"),
        ("format('{:>5}|', 'x')", "    x|"),
        ("format('{:^5}|', 'x')", "  x  |"),
        ("format('{:.2}', 'héllo')", "hé"),
        ("printf('%5s|', 'x')", "    x|"),
        ("printf('%-5s|', 'x')", "x    |"),
        ("printf('%05s|', 'x')", "    x|"),
        ("printf('%.2s', 'héllo')", "hé"),
        ("format('{:5}|', 'é')", "é    |"),
        ("format('{}', true)", "true"),
        ("format('{:5}|', true)", "true |"),
        ("format('{:d}', true)", "1"),
        ("printf('%d', true)", "1"),
        ("printf('%5d|', true)", "    1|"),
        ("printf('%s', true)", "true"),
        ("printf('%x', -1)", "ffffffffffffffff"),
        ("printf('%u', -1)", "18446744073709551615"),
        ("printf('%x', -1::HUGEINT)", "ffffffffffffffffffffffffffffffff"),
        ("format('{:x}', -1)", "-1"),
        ("format('{:#x}', 255)", "0xff"),
        ("format('{:#b}', 5)", "0b101"),
        ("format('{:#o}', 8)", "010"),
        ("format('{:b}', 5::UBIGINT)", "101"),
        ("printf('%hhd', 300)", "44"),
        ("printf('%hd', 70000)", "4464"),
        ("printf('%ld', 1::HUGEINT << 100)", "0"),
        ("printf('%05d', -42)", "-0042"),
        ("printf('%.5d', 42)", "00042"),
        ("printf('%+d', 42)", "+42"),
        ("printf('%c', 65)", "A"),
        ("printf('%5c|', 65)", "    A|"),
        ("format('{}', DATE '2020-01-01')", "2020-01-01"),
        ("format('{:12}|', DATE '2020-01-01')", "2020-01-01  |"),
        ("format('{:.3}', DATE '2020-01-01')", "202"),
        ("printf('%s', DATE '2020-01-01')", "2020-01-01"),
        ("format('{}', [1, 2])", "[1, 2]"),
        ("format('{:_^10}', 'x')", "____x_____"),
        ("printf('%%')", "%"),
    ] {
        assert_eq!(text(&format!("SELECT {expression}")), expected, "{expression}");
    }
}

#[test]
fn a_specifier_of_the_wrong_type_is_an_error() {
    for (expression, expected) in [
        (
            "printf('%s', 0.1::FLOAT)",
            "Invalid type specifier \"s\" for formatting a value of type float",
        ),
        (
            "printf('%d', 1.5::DOUBLE)",
            "Invalid type specifier \"d\" for formatting a value of type float",
        ),
        (
            "format('{:s}', 1.5::DOUBLE)",
            "Invalid type specifier \"s\" for formatting a value of type float",
        ),
        (
            "printf('%s', 1.5)",
            "Invalid type specifier \"s\" for formatting a value of type float",
        ),
        (
            "printf('%f', 1)",
            "Invalid type specifier \"f\" for formatting a value of type int",
        ),
        (
            "printf('%f', 'x')",
            "Invalid type specifier \"f\" for formatting a value of type string",
        ),
        ("format('{:.3}', 1)", "precision not allowed for this argument type"),
        (
            "printf('%s', 1)",
            "Invalid type specifier \"s\" for formatting a value of type int",
        ),
        (
            "format('{:s}', 1)",
            "Invalid type specifier \"s\" for formatting a value of type int",
        ),
        (
            "format('{:s}', true)",
            "Invalid type specifier \"s\" for formatting a value of type int",
        ),
        (
            "printf('%d', DATE '2020-01-01')",
            "Invalid type specifier \"d\" for formatting a value of type string",
        ),
        (
            "format('{:d}', 'x')",
            "Invalid type specifier \"d\" for formatting a value of type string",
        ),
        ("format('{:05}', 'x')", "format specifier requires numeric argument"),
        ("format('{:+}', 'x')", "format specifier requires numeric argument"),
        (
            "format('{:+}', 1::UBIGINT)",
            "format specifier requires signed argument",
        ),
        ("format('{:+}', true)", "format specifier requires signed argument"),
        ("format('{:{<5}', 1)", "invalid fill character '{'"),
        ("format('{:.}', 1.5::DOUBLE)", "missing precision specifier"),
        ("format('{')", "invalid format string"),
        ("format('}')", "unmatched '}' in format string"),
        (
            "format('{0}{}', 1)",
            "cannot switch from manual to automatic argument indexing",
        ),
        (
            "format('{}{0}', 1)",
            "cannot switch from automatic to manual argument indexing",
        ),
        ("format('{1}', 1)", "Argument index \"1\" out of range"),
        (
            "format('{name}', 1)",
            "Argument with name \"name\" not found, did you mean to use it as a format specifier (e.g. {:name}",
        ),
        ("format('{:{}}', 1, -1)", "negative width"),
        ("format('{:{}}', 1, 'x')", "width is not integer"),
        ("format('{:.{}}', 1.5::DOUBLE, 'x')", "precision is not integer"),
        ("format('{:99999999999}', 1)", "number is too big"),
        ("printf('%d')", "Argument index \"0\" out of range"),
        ("printf('%0$d', 1)", "argument index out of range"),
        (
            "printf('%1$d %d', 1)",
            "cannot switch from manual to automatic argument indexing",
        ),
        (
            "printf('%d %1$d', 1)",
            "cannot switch from automatic to manual argument indexing",
        ),
        ("printf('%*d', 'x', 1)", "width is not integer"),
        ("printf('%.*f', 'x', 1.5::DOUBLE)", "precision is not integer"),
        (
            "printf('%c', 200)",
            "Invalid UTF8 produced by format string \"%c\" - note that %c writes a single byte, use chr(...) to write a Unicode code point",
        ),
        ("printf('%l', 1)", "invalid format string"),
    ] {
        let mut connection = Database::open_in_memory().unwrap().connect().unwrap();
        let error = connection.query(&format!("SELECT {expression}")).unwrap_err();
        assert!(error.to_string().contains(expected), "{expression}: {error}");
    }
}

/// DuckDB v1.5.6 printed this as unsigned; the baseline's `arg_converter` keeps the sign.
#[test]
fn a_float_type_with_a_separator_keeps_an_integers_sign() {
    assert_eq!(text("SELECT printf('%,G|', -1)"), "-1|");
}
