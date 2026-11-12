//! `EXISTS` and `NOT EXISTS` in a WHERE clause become SEMI and ANTI joins without the distinct
//! values of the outer query, with a comparison other than an equality as a predicate of the join,
//! and build the smaller of their sides. Their rows must be those of the plan that keeps the
//! distinct values.

use duckrs::{Connection, Database};

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

/// `{o}` is the outer table and `{i}` the one of the subquery.
const QUERIES: &[&str] = &[
    "SELECT count(*), sum(o.v) FROM {o} o WHERE EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE NOT EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v % 3 = 0 AND EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.v > 10)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v % 3 = 0 AND NOT EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.v > 10)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 900 AND EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s <> o.s)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 900 AND NOT EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s <> o.s)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 900 AND EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s < o.s)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 900 AND NOT EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND o.s >= i.s)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 900 AND EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s <> o.s) \
     AND NOT EXISTS (SELECT 1 FROM {i} j WHERE j.k = o.k AND j.s <> o.s AND j.v > 500)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 900 AND EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s = o.s)",
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 900 AND NOT EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s = o.s)",
    // the marker is read by more than a filter
    "SELECT count(*), sum(o.v) FROM {o} o WHERE o.v < 5 OR EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s <> o.s)",
    "SELECT o.k, EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s <> o.s) FROM {o} o WHERE o.v < 50",
    // through a join of the outer query
    "SELECT count(*), sum(o.v) FROM {o} o JOIN {i} p ON o.k = p.k WHERE p.v < 300 \
     AND NOT EXISTS (SELECT 1 FROM {i} i WHERE i.k = o.k AND i.s <> p.s)",
];

#[test]
fn exists_gives_the_rows_of_the_plan_with_distinct_values() {
    let db = Database::open_in_memory().unwrap();
    let mut expected = db.connect().unwrap();
    // NULLs in the keys and in the compared column, of both tables
    expected
        .query(
            "CREATE TABLE small AS SELECT CASE WHEN i % 13 = 0 THEN NULL ELSE i * 7 % 4000 END AS k, \
             CASE WHEN i % 17 = 0 THEN NULL ELSE i % 5 END AS s, i % 1000 AS v FROM range(3000) r(i)",
        )
        .unwrap();
    expected
        .query(
            "CREATE TABLE large AS SELECT CASE WHEN i % 19 = 0 THEN NULL ELSE i % 5000 END AS k, \
             CASE WHEN i % 23 = 0 THEN NULL ELSE i % 3 END AS s, i % 1000 AS v FROM range(200000) r(i)",
        )
        .unwrap();
    expected.query("SET disabled_optimizers='deliminator'").unwrap();
    for threads in [1, 4] {
        let mut actual = db.connect().unwrap();
        actual.query(&format!("PRAGMA threads={threads}")).unwrap();
        for query in QUERIES {
            for (outer, inner) in [("small", "large"), ("large", "small")] {
                let sql = query.replace("{o}", outer).replace("{i}", inner);
                assert_eq!(
                    rows(&mut actual, &sql),
                    rows(&mut expected, &sql),
                    "{sql} on {threads} threads"
                );
            }
        }
    }
}

/// A filter that is an OR of conditions on both sides of a join gives each side the OR of its own
/// parts. The rows must be those of the join without filters pushed down.
#[test]
fn an_or_across_a_join_filters_each_side() {
    let db = Database::open_in_memory().unwrap();
    let mut expected = db.connect().unwrap();
    expected
        .query(
            "CREATE TABLE a AS SELECT i % 500 AS k, CASE WHEN i % 7 = 0 THEN NULL ELSE i % 40 END AS x, \
             'n' || (i % 6) AS s FROM range(3000) r(i)",
        )
        .unwrap();
    expected
        .query(
            "CREATE TABLE b AS SELECT i % 700 AS k, CASE WHEN i % 5 = 0 THEN NULL ELSE i % 30 END AS y, \
             'm' || (i % 4) AS t FROM range(2000) r(i)",
        )
        .unwrap();
    expected.query("SET disabled_optimizers='filter_pushdown'").unwrap();
    let mut actual = db.connect().unwrap();
    for query in [
        "SELECT count(*), sum(a.x), sum(b.y) FROM a, b WHERE a.k = b.k AND \
         ((a.s = 'n1' AND b.t = 'm2' AND a.x < 10) OR (a.s = 'n3' AND b.y BETWEEN 5 AND 9) OR (a.x > 35 AND b.t IN ('m0', 'm3')))",
        // an alternative that says nothing of one side
        "SELECT count(*), sum(a.x), sum(b.y) FROM a, b WHERE a.k = b.k AND ((a.s = 'n1' AND b.y < 3) OR b.t = 'm1')",
        // NULL on one side of an alternative
        "SELECT count(*), sum(a.x), sum(b.y) FROM a JOIN b ON a.k = b.k WHERE (a.x IS NULL AND b.y > 20) OR (a.x = b.y AND a.s = 'n2')",
        "SELECT count(*), sum(a.x), sum(b.y) FROM a LEFT JOIN b ON a.k = b.k AND ((a.x < 5 AND b.y < 5) OR (a.x > 30 AND b.y > 25))",
    ] {
        assert_eq!(rows(&mut actual, query), rows(&mut expected, query), "{query}");
    }
}

/// Join order adds the equalities that follow from others (`a.k = b.k AND a.k = c.k` gives
/// `b.k = c.k`) and keeps one of a set at each join. The rows must be those of the joins as written,
/// also where two columns of one table are in a set, which no join makes equal.
#[test]
fn implied_equalities_keep_the_rows() {
    let db = Database::open_in_memory().unwrap();
    let mut expected = db.connect().unwrap();
    for (table, modulus) in [("a", 50), ("b", 70), ("c", 30), ("d", 40)] {
        expected
            .query(&format!(
                "CREATE TABLE {table} AS SELECT CASE WHEN i % 11 = 0 THEN NULL ELSE i % {modulus} END AS k, \
                 i % 7 AS j, i AS v FROM range(400) r(i)"
            ))
            .unwrap();
    }
    expected.query("SET disabled_optimizers='join_order'").unwrap();
    let mut actual = db.connect().unwrap();
    for query in [
        "SELECT count(*), sum(a.v + b.v + c.v) FROM a, b, c WHERE a.k = b.k AND a.k = c.k",
        "SELECT count(*), sum(a.v + b.v + c.v + d.v) FROM a, b, c, d WHERE a.k = b.k AND b.k = c.k AND c.k = d.k AND d.v < 300",
        // two columns of one table in the set
        "SELECT count(*), sum(a.v + b.v) FROM a JOIN b ON a.k = b.k AND a.k = b.j",
        "SELECT count(*), sum(a.v + b.v + c.v) FROM a, b, c WHERE a.k = b.k AND a.k = b.j AND b.j = c.k",
        "SELECT count(*), sum(a.v + b.v + c.v) FROM a, b, c WHERE a.k = b.k AND a.j = b.k AND a.k = c.k",
        // two sets
        "SELECT count(*), sum(a.v + b.v + c.v) FROM a, b, c WHERE a.k = b.k AND a.k = c.k AND a.j = b.j AND b.j = c.j",
    ] {
        assert_eq!(rows(&mut actual, query), rows(&mut expected, query), "{query}");
    }
}
