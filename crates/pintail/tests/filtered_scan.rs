//! Filters on a table still in its database file are applied while its row groups are read, reading
//! the other columns only for the rows that pass. The rows must be those a scan of the same table in
//! memory gives, on one thread and on several.

use pintail::{Connection, Database};

const ROWS: usize = 400_000;

fn fill(connection: &mut Connection) {
    connection
        .query(&format!(
            "CREATE TABLE t AS SELECT i, i % 97 AS small, CASE WHEN i % 13 = 0 THEN NULL ELSE i * 3 END AS maybe, \
             'group ' || (i % 7) AS label, DATE '1995-01-01' + (i % 1000)::INTEGER AS day, \
             (i % 1000)::DECIMAL(15,2) / 100 AS price FROM range({ROWS}) r(i)"
        ))
        .unwrap();
}

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

#[test]
fn filters_on_a_table_in_its_file_give_the_rows_of_one_in_memory() {
    let directory = std::env::temp_dir().join(format!("pintail-filtered-scan-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("filtered.db");
    let _ = std::fs::remove_file(&path);
    {
        let db = Database::open(&path, false).unwrap();
        let mut connection = db.connect().unwrap();
        fill(&mut connection);
        connection.query("CHECKPOINT").unwrap();
    }
    let memory = Database::open_in_memory().unwrap();
    let mut expected = memory.connect().unwrap();
    fill(&mut expected);
    let file = Database::open(&path, true).unwrap();
    let queries = [
        // selective, on one column, then the others for the rows left
        "SELECT i, maybe, label FROM t WHERE small = 5 ORDER BY i",
        // a conjunction over several columns, most selective last
        "SELECT i, price FROM t WHERE day >= DATE '1995-06-01' AND day < DATE '1995-07-01' AND small < 50 \
         AND price > 1.5 ORDER BY i",
        // nearly every row passes
        "SELECT count(*), sum(maybe), min(label) FROM t WHERE i >= 10",
        // NULLs in a filtered and in a fetched column
        "SELECT count(*), sum(i) FROM t WHERE maybe IS NULL",
        "SELECT i, maybe FROM t WHERE small = 0 AND maybe > 1000 ORDER BY i LIMIT 50",
        // strings, and the row ids of what passes
        "SELECT rowid, i FROM t WHERE label = 'group 3' AND i % 1000 = 3 ORDER BY rowid",
        // nothing passes
        "SELECT count(*) FROM t WHERE small = 1000",
    ];
    for threads in [1, 4] {
        let mut actual = file.connect().unwrap();
        actual.query(&format!("PRAGMA threads={threads}")).unwrap();
        expected.query(&format!("PRAGMA threads={threads}")).unwrap();
        for sql in queries {
            assert_eq!(
                rows(&mut actual, sql),
                rows(&mut expected, sql),
                "{sql} on {threads} threads"
            );
        }
    }
    drop(file);
    let _ = std::fs::remove_dir_all(&directory);
}
