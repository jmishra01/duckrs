//! A GROUP BY past the memory limit spills the rows of the groups it doesn't hold to partitions by the hash of their
//! group, and aggregates each partition afterwards: its groups are those it has without a limit, on one thread and
//! on several, with groups of a number, a string and a list, as the groups in a table of a plain GROUP BY are and
//! as those by key of one with DISTINCT are.

use pintail::{Connection, Database};

fn one(connection: &mut Connection, sql: &str) -> Vec<String> {
    let result = connection.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    (0..result.column_count())
        .map(|column| result.value(column, 0).try_to_string().unwrap())
        .collect()
}

fn run(connection: &mut Connection, sql: &str) {
    connection.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

const QUERIES: &[&str] = &[
    // groups in a table
    "SELECT count(*), sum(c), sum(total), sum(hash(k, s, l, c, total, low, high)) FROM (SELECT k, s, l, count(*) AS c, \
     sum(v) AS total, min(v) AS low, max(s) AS high FROM t GROUP BY k, s, l)",
    // groups by key: DISTINCT keeps the values it has seen by group
    "SELECT count(*), sum(c), sum(hash(k, s, c)) FROM (SELECT k, s, count(DISTINCT v % 5) AS c FROM t GROUP BY k, s)",
];

#[test]
fn groups_past_the_memory_limit_are_those_without_one() {
    let directory = std::env::temp_dir().join(format!("pintail-aggregate-spill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let db = Database::open_in_memory().unwrap();
    let mut c = db.connect().unwrap();
    run(&mut c, &format!("SET temp_directory = '{}'", directory.display()));
    run(
        &mut c,
        "CREATE TABLE t AS SELECT i % 300000 AS k, 'a group named number ' || (i % 300000 % 1000) AS s, \
         [i % 300000 % 7, i % 3] AS l, i AS v FROM range(1000000) r(i)",
    );
    for query in QUERIES {
        for threads in [1, 8] {
            run(&mut c, &format!("SET threads = {threads}"));
            run(&mut c, "RESET memory_limit");
            let unlimited = one(&mut c, query);
            run(&mut c, "SET memory_limit = '30MB'");
            let limited = one(&mut c, query);
            assert_eq!(limited, unlimited, "{query} on {threads} threads");
            assert_eq!(unlimited[0], "300000", "{query}");
        }
    }
    run(&mut c, "RESET memory_limit");
    std::fs::remove_dir_all(&directory).unwrap();
}
