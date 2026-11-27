//! `Connection::get_table_names` (`ClientContext::GetTableNames`): the tables a statement reads, bound in
//! `BindingMode::EXTRACT_NAMES`, so that tables the catalog does not have count too.

use duckrs::Database;

#[test]
fn table_names_of_tables_that_are_not_there() {
    let database = Database::open_in_memory().unwrap();
    let mut connection = database.connect().unwrap();
    let names = connection
        .get_table_names("SELECT a, lower(b) FROM t1 JOIN s.t2 ON t1.x = t2.y WHERE c > 1", false)
        .unwrap();
    assert_eq!(names, ["t1", "t2"]);
    let names = connection
        .get_table_names("SELECT * FROM t1 AS x, s.\"t.2\"", true)
        .unwrap();
    assert_eq!(names, ["s.\"t.2\"", "t1 AS x"]);
}

#[test]
fn table_names_look_through_views_and_skip_ctes() {
    let database = Database::open_in_memory().unwrap();
    let mut connection = database.connect().unwrap();
    connection.query("CREATE TABLE base(i INTEGER)").unwrap();
    connection.query("CREATE VIEW v AS SELECT i FROM base").unwrap();
    let names = connection
        .get_table_names("WITH c AS (SELECT 1) SELECT * FROM v, c, (SELECT count(*) FROM other)", false)
        .unwrap();
    assert_eq!(names, ["base", "other"]);
    assert!(connection.get_table_names("SELECT 1; SELECT 2", false).is_err());
}

#[test]
fn normal_binding_is_unchanged() {
    let database = Database::open_in_memory().unwrap();
    let mut connection = database.connect().unwrap();
    connection.get_table_names("SELECT * FROM missing", false).unwrap();
    let error = connection.query("SELECT * FROM missing").unwrap_err();
    assert!(error.message.contains("does not exist"), "{error}");
}
