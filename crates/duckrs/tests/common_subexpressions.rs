//! An expression that a projection or an aggregate computes more than once is computed once, in a
//! projection below it (`CommonSubExpressionOptimizer`), and the results are those without that.

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

fn plan(connection: &mut Connection, sql: &str) -> String {
    rows(connection, &format!("EXPLAIN {sql}")).concat().concat()
}

const QUERIES: &[&str] = &[
    // Q01's shape: a product in two sums, by group
    "SELECT g, sum(p * (1 - d)), sum(p * (1 - d) * (1 + t)), count(*) FROM t GROUP BY g ORDER BY g",
    // an ungrouped aggregate, whose filters stay with it
    "SELECT sum(p * 2) FILTER (WHERE p * 2 > 10), sum(p * 2) FILTER (WHERE p * 2 > 10), sum(p * 2) FROM t",
    // a projection
    "SELECT (p * 2) + (p * 2), p FROM t ORDER BY 2",
    // inside TRY the errors stay where they are, and a CASE's branches may not run
    "SELECT TRY(p // (d - 1) + p // (d - 1)), CASE WHEN d > 1 THEN p // d ELSE 0 END + p // (d + 1) FROM t ORDER BY 1, 2",
    // a COALESCE operand past the first runs only where those before it are NULL
    "SELECT COALESCE(CASE WHEN d = 0 THEN 1 END, (p // d) IN (1, 3, 7, 11, 13)) FROM t ORDER BY 1",
];

#[test]
fn common_subexpressions_are_computed_once() {
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE TABLE t AS SELECT i % 3 AS g, i::DECIMAL(15, 2) AS p, (i % 4)::DECIMAL(15, 2) AS d, \
         (i % 5)::DECIMAL(15, 2) AS t FROM range(1000) r(i)",
    )
    .unwrap();
    let explained = plan(&mut c, QUERIES[0]);
    let aggregate = explained.find("Hash Group By").expect("an aggregate");
    let projection = explained[aggregate..]
        .find("Projection")
        .expect("a projection below the aggregate");
    assert!(explained[aggregate..][..projection].contains("sum(#"), "{explained}");
    for query in QUERIES {
        c.query("RESET disabled_optimizers").unwrap();
        let extracted = rows(&mut c, query);
        c.query("SET disabled_optimizers = 'common_subexpressions'").unwrap();
        assert_eq!(extracted, rows(&mut c, query), "{query}");
    }
}
