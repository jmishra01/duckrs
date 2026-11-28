//! A scan reads only the columns the query uses, also below the filter of a long IN list, which becomes a MARK join
//! whose marker a column select hides, and below a NOT EXISTS (Q22's shape); the results are those of the query
//! without the rewrite.

use pintail::{Connection, Database};

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
fn a_scan_below_an_in_list_reads_the_columns_used() {
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE TABLE c AS SELECT i AS k, 'phone ' || (i % 40) AS phone, i % 7 AS balance, repeat('x', 50) AS comment \
         FROM range(5000) r(i)",
    )
    .unwrap();
    c.query("CREATE TABLE o AS SELECT i * 3 AS ck FROM range(3000) r(i)")
        .unwrap();
    let query = "SELECT count(*), sum(balance) FROM c WHERE phone IN ('phone 1', 'phone 3', 'phone 5', 'phone 7', \
                 'phone 9', 'phone 11', 'phone 13') AND NOT EXISTS (SELECT * FROM o WHERE ck = k)";
    let plan = rows(&mut c, &format!("EXPLAIN {query}")).concat().concat();
    assert!(!plan.contains("comment"), "{plan}");
    let rewritten = rows(&mut c, query);
    c.query("SET disabled_optimizers = 'in_clause'").unwrap();
    assert_eq!(rewritten, rows(&mut c, query));
}
