//! The engine's kernels, each through the SQL that reaches it on a million rows: scans and filters,
//! hash aggregates of few and many groups, the hash join, sorting, top-N, windows, string matching
//! and casts. `benchmark_runner` (tools/benchmark) times DuckDB's own `.benchmark` files; these are
//! the quick loop for working on one kernel.
//!
//! ```text
//! cargo bench -p pintail --bench kernels [filter]
//! ```

use criterion::{Criterion, criterion_group, criterion_main};
use pintail::{Connection, Database};
use std::hint::black_box;

const ROWS: usize = 1_000_000;

fn setup() -> (Database, Connection) {
    let database = Database::open_in_memory().expect("an in-memory database");
    let mut connection = database.connect().expect("a connection");
    connection
        .query(&format!(
            "CREATE TABLE t AS SELECT i, i % 7 AS small, i % 100000 AS large, (i * 7919) % {ROWS} AS shuffled, \
             i::DOUBLE / 3 AS d, 'value ' || (i % 1000) AS s, DATE '2000-01-01' + (i % 3650)::INTEGER AS day \
             FROM range({ROWS}) r(i)"
        ))
        .expect("the table");
    connection
        .query("CREATE TABLE dim AS SELECT i AS k, 'name ' || i AS name FROM range(100000) r(i)")
        .expect("the dimension table");
    (database, connection)
}

fn bench(c: &mut Criterion) {
    let (_database, mut connection) = setup();
    let cases: &[(&str, &str)] = &[
        ("scan: sum of a column", "SELECT sum(i) FROM t"),
        (
            "filter: range on an integer",
            "SELECT count(*) FROM t WHERE shuffled BETWEEN 1000 AND 500000",
        ),
        (
            "filter: equality on a string",
            "SELECT count(*) FROM t WHERE s = 'value 42'",
        ),
        ("filter: LIKE prefix", "SELECT count(*) FROM t WHERE s LIKE 'value 9%'"),
        (
            "aggregate: 7 groups",
            "SELECT small, sum(d), count(*) FROM t GROUP BY small",
        ),
        ("aggregate: 100k groups", "SELECT large, sum(i) FROM t GROUP BY large"),
        ("aggregate: count distinct", "SELECT count(DISTINCT large) FROM t"),
        (
            "join: hash join with 100k build rows",
            "SELECT count(*) FROM t JOIN dim ON t.large = dim.k",
        ),
        (
            "sort: a million integers",
            "SELECT shuffled FROM t ORDER BY shuffled OFFSET 999999",
        ),
        (
            "top-n: 10 of a million",
            "SELECT shuffled FROM t ORDER BY shuffled DESC LIMIT 10",
        ),
        (
            "window: running sum per group",
            "SELECT sum(w) FROM (SELECT sum(i) OVER (PARTITION BY small ORDER BY i) AS w FROM t)",
        ),
        ("cast: integer to varchar", "SELECT sum(length(i::VARCHAR)) FROM t"),
        ("date: year()", "SELECT sum(year(day)) FROM t"),
    ];
    let mut group = c.benchmark_group("kernels");
    group.sample_size(10);
    for (name, sql) in cases {
        // a wrong query should fail here, not show up as a fast benchmark
        connection.query(sql).unwrap_or_else(|e| panic!("{name}: {e}"));
        group.bench_function(*name, |b| {
            b.iter(|| black_box(connection.query(sql).expect("the query")))
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
