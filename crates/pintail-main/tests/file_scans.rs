//! Queries over a database file that C++ DuckDB wrote: they read what they need of the file, and
//! keep the tables in it. Moved from `pintail-core`'s storage tests, as they run SQL end to end.

use pintail_main::{ClientContext, DatabaseInstance};
use std::path::Path;

/// A query reads what it needs of a file: all of this runs without the rows in memory.
#[test]
fn tables_stay_in_the_file() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pintail/tests/fixtures/scan_v122880.db");
    let db = DatabaseInstance::open_file(&path, true).unwrap();
    let mut context = ClientContext::new(db.clone());
    let in_file = |table: &str| db.catalog.get_table(None, table).unwrap().source().is_some();
    for sql in [
        "SELECT count(*) FROM big",
        "SELECT i, s FROM big WHERE i = 7",
        "SELECT tiny, sum(i) FROM big GROUP BY tiny",
        "SELECT count(*) FROM big a JOIN holes b ON a.i = b.i",
        "SELECT * FROM big ORDER BY i LIMIT 5",
        "BEGIN",
        "SELECT count(*) FROM holes",
        "ROLLBACK",
        "DESCRIBE big",
        "SELECT * FROM duckdb_tables()",
    ] {
        context.query(sql).unwrap();
        assert!(in_file("big"), "{sql}");
        assert!(in_file("holes"), "{sql}");
    }
}

/// The row groups a filter skips come from the metadata, so skipping them takes no reading.
#[test]
fn filters_skip_row_groups_of_a_file() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pintail/tests/fixtures/scan_v122880.db");
    let mut context = ClientContext::new(DatabaseInstance::open_file(&path, true).unwrap());
    for (filter, groups) in [("i < 100", "1 / 2"), ("i >= 125000", "1 / 2"), ("i >= 100", "2 / 2")] {
        let sql = format!("EXPLAIN ANALYZE SELECT count(*) FROM big WHERE {filter}");
        let plan = context.query(&sql).unwrap().value(1, 0).try_to_string().unwrap();
        let line = plan
            .lines()
            .find(|line| line.contains("Row Groups Scanned"))
            .unwrap_or(&plan);
        assert!(line.contains(groups), "{filter}: {plan}");
    }
}
