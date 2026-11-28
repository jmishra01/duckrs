//! A hash join gives the scan below its probe side a filter of its build keys. The rows of a join
//! must be those of the same join on `k + 0`, which no filter can reach, for each kind of join a
//! filter goes through, on tables in memory and in a file, on one thread and on several.

use pintail::{Connection, Database};

fn fill(connection: &mut Connection) {
    connection
        .query(
            "CREATE TABLE probe AS SELECT i AS k, CASE WHEN i % 11 = 0 THEN NULL ELSE i % 5000 END AS m, \
             (i % 1000)::BIGINT AS w, DATE '1990-01-01' + (i % 4000)::INTEGER AS d FROM range(300000) r(i)",
        )
        .unwrap();
    connection
        .query(
            "CREATE TABLE build AS SELECT i * 37 AS k, CASE WHEN i % 9 = 0 THEN NULL ELSE i * 3 END AS m, \
             (i * 7)::BIGINT AS w, DATE '1990-01-01' + (i * 13)::INTEGER AS d FROM range(2000) r(i)",
        )
        .unwrap();
}

fn rows(connection: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let result = connection.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut rows: Vec<Vec<String>> = (0..result.row_count())
        .map(|row| {
            (0..result.column_count())
                .map(|column| result.value(column, row).try_to_string().unwrap())
                .collect()
        })
        .collect();
    rows.sort();
    rows
}

/// Each query with `{k}` for the probe key, which is `probe.<column>` with a filter and
/// `probe.<column> + 0` without one.
const QUERIES: &[(&str, &str)] = &[
    (
        "k",
        "SELECT count(*), sum(probe.w) FROM probe JOIN build ON {k} = build.k",
    ),
    (
        "m",
        "SELECT probe.m, count(*) FROM probe JOIN build ON {k} = build.m GROUP BY ALL",
    ),
    (
        "w",
        "SELECT count(*) FROM probe JOIN build ON {k} = build.w AND probe.k % 3 = 0",
    ),
    (
        "d",
        "SELECT count(*), min(probe.k) FROM probe, build WHERE {k} = build.d",
    ),
    ("k", "SELECT count(*) FROM probe WHERE {k} IN (SELECT k FROM build)"),
    (
        "m",
        "SELECT count(*) FROM probe WHERE EXISTS (SELECT 1 FROM build WHERE build.m = {k})",
    ),
    (
        "m",
        "SELECT count(*) FROM probe WHERE {k} NOT IN (SELECT m FROM build WHERE m IS NOT NULL)",
    ),
    (
        "k",
        "SELECT count(*), count(build.k) FROM probe LEFT JOIN build ON {k} = build.k",
    ),
    (
        "k",
        "SELECT count(*), count(probe.k) FROM build LEFT JOIN probe ON build.k = {k}",
    ),
    (
        "m",
        "SELECT count(*), count(probe.k) FROM probe RIGHT JOIN build ON {k} = build.m",
    ),
    (
        "m",
        "SELECT count(*), count(build.m) FROM probe FULL JOIN build ON {k} = build.m",
    ),
    (
        "m",
        "SELECT count(*) FROM probe JOIN build ON {k} IS NOT DISTINCT FROM build.m",
    ),
    // through another join, on either of its sides
    (
        "k",
        "SELECT count(*) FROM (SELECT probe.k AS pk, b2.k AS bk FROM probe JOIN build b2 ON probe.w = b2.w) t \
         JOIN build ON t.pk = build.k",
    ),
    (
        "k",
        "SELECT count(*) FROM probe JOIN (SELECT p2.k FROM probe p2 JOIN build b2 ON p2.k = b2.k) t \
         ON {k} = t.k",
    ),
];

fn check(file: &Database, expected: &mut Connection) {
    for threads in [1, 4] {
        let mut actual = file.connect().unwrap();
        actual.query(&format!("PRAGMA threads={threads}")).unwrap();
        expected.query(&format!("PRAGMA threads={threads}")).unwrap();
        for (column, query) in QUERIES {
            let filtered = query.replace("{k}", &format!("probe.{column}"));
            let plain = query.replace("{k}", &format!("(probe.{column} + 0)"));
            assert_eq!(
                rows(&mut actual, &filtered),
                rows(expected, &plain),
                "{filtered} on {threads} threads"
            );
        }
    }
}

#[test]
fn joins_with_filters_give_the_rows_of_joins_without() {
    let memory = Database::open_in_memory().unwrap();
    let mut expected = memory.connect().unwrap();
    fill(&mut expected);
    check(&memory, &mut expected);

    let directory = std::env::temp_dir().join(format!("pintail-join-filter-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("join_filter.db");
    let _ = std::fs::remove_file(&path);
    {
        let db = Database::open(&path, false).unwrap();
        let mut connection = db.connect().unwrap();
        fill(&mut connection);
        connection.query("CHECKPOINT").unwrap();
    }
    let file = Database::open(&path, true).unwrap();
    check(&file, &mut expected);
    drop(file);
    let _ = std::fs::remove_dir_all(&directory);
}
