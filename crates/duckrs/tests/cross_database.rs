//! Statements that write one database and read others: they run in the database they write, and the
//! tables of the others bind by their names. Unqualified names stay in the default database, as along
//! C++'s search path, and a transaction still writes to one database only.

use duckrs::{Connection, Database};

fn rows(connection: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let result = connection.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    (0..result.row_count())
        .map(|row| {
            (0..result.column_count())
                .map(|column| result.value(column, row).try_to_string().unwrap())
                .collect()
        })
        .collect()
}

fn run(connection: &mut Connection, sql: &str) {
    connection.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn one(connection: &mut Connection, sql: &str) -> String {
    rows(connection, sql).remove(0).remove(0)
}

fn connection() -> Connection {
    let database = Database::open_in_memory().unwrap();
    let mut connection = database.connect().unwrap();
    for sql in [
        "ATTACH ':memory:' AS db1",
        "ATTACH ':memory:' AS db2",
        "CREATE TABLE db1.t AS SELECT range::INTEGER i FROM range(5)",
        "CREATE TABLE db2.u (i INTEGER)",
        "CREATE TABLE local AS SELECT 7 i",
    ] {
        run(&mut connection, sql);
    }
    connection
}

#[test]
fn writes_read_another_database() {
    let mut c = connection();
    run(&mut c, "INSERT INTO db2.u SELECT i FROM db1.t WHERE i < 3");
    assert_eq!(one(&mut c, "SELECT sum(i) FROM db2.u"), "3");
    run(&mut c, "CREATE TABLE db2.v AS SELECT i * 10 AS j FROM db1.main.t");
    assert_eq!(one(&mut c, "SELECT sum(j) FROM db2.v"), "100");
    run(
        &mut c,
        "UPDATE db2.u SET i = i + 100 WHERE i IN (SELECT i FROM db1.t WHERE i > 0)",
    );
    assert_eq!(one(&mut c, "SELECT sum(i) FROM db2.u"), "203");
    run(&mut c, "DELETE FROM db2.u WHERE i IN (SELECT i + 100 FROM db1.t)");
    assert_eq!(one(&mut c, "SELECT count(*) FROM db2.u"), "1");
    // nothing went to the database that was read
    assert_eq!(one(&mut c, "SELECT count(*) FROM db1.t"), "5");
}

#[test]
fn unqualified_names_stay_in_the_default_database() {
    let mut c = connection();
    run(&mut c, "CREATE TABLE db2.w AS SELECT i FROM local");
    assert_eq!(one(&mut c, "SELECT i FROM db2.w"), "7");
    run(
        &mut c,
        "INSERT INTO db2.u SELECT l.i + t.i FROM local l, db1.t t WHERE t.i = 1",
    );
    assert_eq!(one(&mut c, "SELECT i FROM db2.u"), "8");
    // a CTE is not a table of the default database
    run(
        &mut c,
        "INSERT INTO db2.u WITH local AS (SELECT 1 i) SELECT i FROM local",
    );
    assert_eq!(one(&mut c, "SELECT count(*) FROM db2.u WHERE i = 1"), "1");
    // the view of an attached database finds what its database lacks in the default one
    run(&mut c, "CREATE VIEW db2.lv AS SELECT i FROM local");
    assert_eq!(one(&mut c, "SELECT i FROM db2.lv"), "7");
}

/// Tables of one name and schema in two databases: their bindings tell them apart by the catalog
/// (`BindingAlias`), which a column, a star and a struct pack can name.
#[test]
fn columns_qualified_by_the_database() {
    let mut c = connection();
    for sql in [
        "CREATE SCHEMA db1.s1",
        "CREATE SCHEMA db2.s1",
        "CREATE TABLE db1.s1.x (c INTEGER, sq AS (c * c))",
        "CREATE TABLE db2.s1.x (c INTEGER)",
        "INSERT INTO db1.s1.x VALUES (3)",
        "INSERT INTO db2.s1.x VALUES (5)",
    ] {
        run(&mut c, sql);
    }
    let from = "FROM db1.s1.x, db2.s1.x";
    assert_eq!(rows(&mut c, &format!("SELECT * {from}")), [["3", "9", "5"]]);
    assert_eq!(rows(&mut c, &format!("SELECT db1.x.c, db2.x.c {from}")), [["3", "5"]]);
    assert_eq!(
        rows(&mut c, &format!("SELECT db2.s1.x.c, db1.s1.x.sq {from}")),
        [["5", "9"]]
    );
    assert_eq!(
        rows(&mut c, &format!("SELECT * EXCLUDE (db1.s1.x.c, sq) {from}")),
        [["5"]]
    );
    assert_eq!(rows(&mut c, &format!("SELECT db2.s1.x {from}")), [["{'c': 5}"]]);
    let error = c.query(&format!("SELECT s1.x.c {from}")).unwrap_err().to_string();
    assert!(error.contains("Ambiguous reference to table"), "{error}");
    let error = c.query(&format!("SELECT c {from}")).unwrap_err().to_string();
    assert!(error.contains("'db1.s1.x.c' or 'db2.s1.x.c'"), "{error}");
}

#[test]
fn a_transaction_writes_one_database() {
    let mut c = connection();
    run(&mut c, "BEGIN");
    run(&mut c, "INSERT INTO db2.u SELECT i FROM db1.t WHERE i = 4");
    let error = c.query("INSERT INTO db1.t SELECT i FROM db2.u").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("a single transaction can only write to a single attached database"),
        "{error}"
    );
    run(&mut c, "ROLLBACK");
    assert_eq!(one(&mut c, "SELECT count(*) FROM db2.u"), "0");
}
