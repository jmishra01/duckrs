//! `format` and `printf` take a FLOAT as the DOUBLE it widens to, so they print digits that a
//! cast to VARCHAR does not. The expected text comes from DuckDB v1.5.6.

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
