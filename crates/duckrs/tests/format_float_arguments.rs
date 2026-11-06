//! `format` and `printf` take a FLOAT or a DECIMAL as a DOUBLE, so a FLOAT prints digits that a
//! cast to VARCHAR does not, and write it as fmt does. The expected text comes from DuckDB v1.5.6.

use duckrs::Database;

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
    ] {
        let mut connection = Database::open_in_memory().unwrap().connect().unwrap();
        let error = connection.query(&format!("SELECT {expression}")).unwrap_err();
        assert!(error.to_string().contains(expected), "{expression}: {error}");
    }
}
