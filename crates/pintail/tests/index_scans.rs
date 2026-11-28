//! An index scan that misses a row gives a wrong answer without an error, so each query here is
//! answered both ways, and the two have to agree.

use pintail::{Connection, Database};

fn run(connection: &mut Connection, sql: &str) {
    connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
}

fn rows(connection: &mut Connection, sql: &str) -> Vec<String> {
    let result = connection.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let mut rows: Vec<String> = (0..result.row_count())
        .map(|row| {
            let fields: Vec<String> = result
                .collection
                .row(row)
                .iter()
                .map(|v| {
                    if v.is_null() {
                        "NULL".to_string()
                    } else {
                        v.try_to_string().unwrap()
                    }
                })
                .collect();
            fields.join("|")
        })
        .collect();
    rows.sort();
    rows
}

/// Whether an EXPLAIN ANALYZE of `sql` says that an index answered it.
fn used_index(connection: &mut Connection, sql: &str) -> bool {
    rows(connection, &format!("EXPLAIN ANALYZE {sql}"))
        .iter()
        .any(|row| row.contains("Type: Index Scan"))
}

/// Answers `sql` with the index, as far as it applies, and without.
fn same_both_ways(connection: &mut Connection, sql: &str) -> bool {
    run(connection, "SET index_scan_percentage = 1.0");
    run(connection, "SET index_scan_max_count = 1000000000");
    let indexed = used_index(connection, sql);
    let with_index = rows(connection, sql);
    run(connection, "SET index_scan_percentage = 0");
    run(connection, "SET index_scan_max_count = 0");
    assert!(
        !used_index(connection, sql) || with_index.is_empty(),
        "{sql} ignores the limit"
    );
    let without_index = rows(connection, sql);
    assert_eq!(with_index, without_index, "{sql}");
    indexed
}

/// xorshift, as all that matters is that a failure comes back.
struct Random(u64);

impl Random {
    fn next(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// The type of a column, what values it has, and what a predicate compares it with.
const COLUMNS: &[(&str, &[&str])] = &[
    ("TINYINT", &["-128", "-1", "0", "1", "7", "127", "NULL"]),
    (
        "SMALLINT",
        &["-32768", "-300", "-1", "0", "255", "256", "32767", "NULL"],
    ),
    (
        "INTEGER",
        &[
            "-2147483648",
            "-65536",
            "-1",
            "0",
            "1",
            "255",
            "256",
            "65535",
            "65536",
            "2147483647",
            "NULL",
        ],
    ),
    (
        "BIGINT",
        &[
            "-9223372036854775808",
            "-1",
            "0",
            "1",
            "4294967296",
            "9223372036854775807",
            "NULL",
        ],
    ),
    (
        "HUGEINT",
        &[
            "-170141183460469231731687303715884105728",
            "-1",
            "0",
            "18446744073709551616",
            "NULL",
        ],
    ),
    ("UTINYINT", &["0", "1", "127", "128", "255", "NULL"]),
    (
        "UBIGINT",
        &["0", "1", "9223372036854775808", "18446744073709551615", "NULL"],
    ),
    (
        "FLOAT",
        &["'-inf'", "-1.5", "-0.0", "0.0", "1.5", "'inf'", "'nan'", "NULL"],
    ),
    (
        "DOUBLE",
        &[
            "'-inf'", "-1e300", "-1.5", "-0.0", "0.0", "5e-324", "1.5", "'inf'", "'nan'", "NULL",
        ],
    ),
    ("DECIMAL(9,3)", &["-1.5", "0", "0.001", "1.5", "999999.999", "NULL"]),
    ("DECIMAL(30,5)", &["-1.5", "0", "1234567890123456.12345", "NULL"]),
    (
        "DATE",
        &["'1970-01-01'", "'0001-01-01'", "'2024-02-29'", "'9999-12-31'", "NULL"],
    ),
    (
        "TIMESTAMP",
        &["'1970-01-01 00:00:00'", "'2024-02-29 12:34:56.789'", "NULL"],
    ),
    ("BOOLEAN", &["true", "false", "NULL"]),
    (
        "UUID",
        &[
            "'00000000-0000-0000-0000-000000000000'",
            "'ffffffff-ffff-ffff-ffff-ffffffffffff'",
            "'12345678-1234-1234-1234-123456789abc'",
            "NULL",
        ],
    ),
    (
        "VARCHAR",
        &[
            "''",
            "'a'",
            "'a '",
            "'aa'",
            "'ab'",
            "'b'",
            "'Z'",
            "'é'",
            "'日本語'",
            "'a' || chr(1) || 'b'",
            "repeat('x', 300)",
            "repeat('x', 301)",
            "NULL",
        ],
    ),
    (
        "BLOB",
        &[
            "''::BLOB",
            "'\\x00'::BLOB",
            "'\\x00\\x00'::BLOB",
            "'\\x00\\x01'::BLOB",
            "'\\x01'::BLOB",
            "'\\xFF'::BLOB",
            "NULL",
        ],
    ),
];

fn predicates(value: &str, other: &str, ty: &str) -> Vec<String> {
    let v = format!("CAST({value} AS {ty})");
    let o = format!("CAST({other} AS {ty})");
    vec![
        format!("c = {v}"),
        format!("{v} = c"),
        format!("c <> {v}"),
        format!("c < {v}"),
        format!("c <= {v}"),
        format!("c > {v}"),
        format!("c >= {v}"),
        format!("{v} < c"),
        format!("c BETWEEN {v} AND {o}"),
        format!("c > {v} AND c <= {o}"),
        format!("c >= {v} AND c < {o}"),
        format!("c IN ({v}, {o})"),
        format!("c NOT IN ({v}, {o})"),
        format!("c IS NOT DISTINCT FROM {v}"),
        format!("c IS DISTINCT FROM {v}"),
        format!("c = {v} OR c = {o}"),
        format!("c = {v} AND c > {o}"),
        format!("c IS NULL"),
        format!("c IS NOT NULL"),
    ]
}

/// Every type: compared with all of its values, with duplicates and NULLs, through changes.
#[test]
fn an_index_finds_what_a_scan_finds() {
    let database = Database::open_in_memory().unwrap();
    let mut connection = database.connect().unwrap();
    let mut random = Random(0x1234_5678_9abc_def1);
    let mut index_scans = 0;
    for (number, (ty, values)) in COLUMNS.iter().enumerate() {
        let table = format!("t{number}");
        let select = |predicate: &str| format!("SELECT id, c FROM {table} WHERE {predicate}");
        run(&mut connection, &format!("CREATE TABLE {table} (id INTEGER, c {ty})"));
        let insert = |id: usize, value: &str| format!("INSERT INTO {table} VALUES ({id}, {value})");
        for id in 0..60 {
            run(&mut connection, &insert(id, values[random.next(values.len())]));
        }
        run(&mut connection, &format!("CREATE INDEX i{number} ON {table} (c)"));
        for round in 0..4 {
            for value in values.iter().filter(|value| **value != "NULL") {
                let other = values[random.next(values.len())];
                let other = if other == "NULL" { values[0] } else { other };
                for predicate in predicates(value, other, ty) {
                    index_scans += usize::from(same_both_ways(&mut connection, &select(&predicate)));
                }
            }
            // what the trees have to follow: rows added, moved and gone, also when it's undone
            let id = 1000 + round;
            run(&mut connection, &insert(id, values[random.next(values.len())]));
            run(
                &mut connection,
                &format!("DELETE FROM {table} WHERE id % 11 = {}", round % 11),
            );
            run(&mut connection, "BEGIN");
            run(&mut connection, &insert(id + 100, values[random.next(values.len())]));
            let value = values[random.next(values.len())];
            run(
                &mut connection,
                &format!("UPDATE {table} SET c = {value} WHERE id % 7 = {round}"),
            );
            for predicate in predicates(values[0], values[0], ty).iter().take(4) {
                if values[0] != "NULL" {
                    same_both_ways(&mut connection, &select(predicate));
                }
            }
            run(&mut connection, if round % 2 == 0 { "COMMIT" } else { "ROLLBACK" });
        }
    }
    assert!(index_scans > 500, "only {index_scans} queries went through an index");
}

/// A key and an expression, which a scan has to take the same way as a column.
#[test]
fn keys_of_other_kinds_find_the_same() {
    let database = Database::open_in_memory().unwrap();
    let mut connection = database.connect().unwrap();
    run(
        &mut connection,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR UNIQUE, a INTEGER, b INTEGER, n VARCHAR)",
    );
    run(
        &mut connection,
        "INSERT INTO t SELECT i, 'name ' || i, i % 10, i % 3, CASE WHEN i % 4 = 0 THEN NULL ELSE 'x' || (i % 5) END \
         FROM range(5000) r(i)",
    );
    run(&mut connection, "CREATE INDEX by_a ON t (a)");
    run(&mut connection, "CREATE INDEX by_ab ON t (a, b)");
    run(&mut connection, "CREATE INDEX by_sum ON t ((a + 1))");
    run(&mut connection, "CREATE INDEX by_lower ON t (lower(n))");
    for predicate in [
        "id = 77",
        "id >= 4990",
        "id < 3",
        "name = 'name 33'",
        "a = 3",
        "a + 1 = 4",
        "a + 1 >= 9",
    ] {
        let sql = format!("SELECT * FROM t WHERE {predicate}");
        assert!(same_both_ways(&mut connection, &sql), "{sql}");
    }
    for predicate in [
        "lower(n) = 'x1'",
        "a = 3 AND b = 1",
        "a = 3 OR b = 1",
        "n = 'x2'",
        "id + 1 = 5",
    ] {
        same_both_ways(&mut connection, &format!("SELECT * FROM t WHERE {predicate}"));
    }
    // what writes find through it
    run(&mut connection, "SET index_scan_percentage = 1.0");
    run(&mut connection, "DELETE FROM t WHERE id = 100");
    run(&mut connection, "UPDATE t SET b = 9 WHERE a = 2");
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM t"), ["4999"]);
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM t WHERE b = 9"), ["500"]);
    assert_eq!(rows(&mut connection, "SELECT count(*) FROM t WHERE id = 100"), ["0"]);
}

/// A transaction looks through a tree of what it sees, and doesn't leave it for anyone else.
#[test]
fn a_transaction_finds_its_own_rows() {
    let database = Database::open_in_memory().unwrap();
    let mut first = database.connect().unwrap();
    let mut second = database.connect().unwrap();
    run(&mut first, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)");
    run(&mut first, "INSERT INTO t SELECT i, i FROM range(1000) r(i)");
    run(&mut first, "BEGIN");
    assert_eq!(rows(&mut first, "SELECT v FROM t WHERE id = 5"), ["5"]);
    run(&mut first, "INSERT INTO t VALUES (2000, 1)");
    assert_eq!(rows(&mut first, "SELECT v FROM t WHERE id = 2000"), ["1"]);
    run(&mut first, "DELETE FROM t WHERE id = 7");
    assert!(rows(&mut first, "SELECT v FROM t WHERE id = 7").is_empty());
    run(&mut second, "UPDATE t SET v = 99 WHERE id = 5");
    assert!(rows(&mut second, "SELECT v FROM t WHERE id = 2000").is_empty());
    assert_eq!(rows(&mut second, "SELECT v FROM t WHERE id = 7"), ["7"]);
    assert_eq!(rows(&mut second, "SELECT v FROM t WHERE id = 5"), ["99"]);
    assert_eq!(rows(&mut first, "SELECT v FROM t WHERE id = 5"), ["5"]);
    run(&mut first, "COMMIT");
    assert_eq!(rows(&mut second, "SELECT v FROM t WHERE id = 5"), ["99"]);
    assert_eq!(rows(&mut second, "SELECT v FROM t WHERE id = 2000"), ["1"]);
    assert!(rows(&mut second, "SELECT v FROM t WHERE id = 7").is_empty());
}

/// With values to look up, what else a filter says on the column is left to the scan, which only
/// works if the values are all that could match.
#[test]
fn values_go_with_other_filters() {
    let db = Database::open_in_memory().unwrap();
    let mut connection = db.connect().unwrap();
    run(
        &mut connection,
        "CREATE TABLE t AS SELECT i AS id, i % 5 AS v FROM range(20000) r(i)",
    );
    run(&mut connection, "CREATE INDEX t_id ON t (id)");
    run(&mut connection, "INSERT INTO t VALUES (NULL, 1), (NULL, 2)");
    run(&mut connection, "DELETE FROM t WHERE id % 97 = 0");
    for predicate in [
        "id IN (1, 42, 97, 100, 42) AND id != 42",
        "id IN (1, 42, 100) AND id <= 42",
        "id IN (1, 42, 100) AND id > 1 AND id < 100",
        "id IN (1, 42) AND id IS DISTINCT FROM 1",
        "id IN (1, 42) AND id IS NOT DISTINCT FROM 1",
        "id IN (1, 42) AND (id = 1 OR id > 100)",
        "(id = 1 OR id > 100) AND id = 200",
    ] {
        let sql = format!("SELECT id, v FROM t WHERE {predicate}");
        assert!(same_both_ways(&mut connection, &sql), "{sql} uses no index");
    }
    // parts of a condition that can't be looked up don't make the rest values
    for predicate in [
        "id = 1 OR id > 100",
        "id = 1 OR id IS NULL",
        "id = 1 OR v = 2",
        "id = 42 AND id = 43",
        "id = 42 AND id != 42",
        "id IN (1, 42) AND id BETWEEN 2 AND 50",
    ] {
        same_both_ways(&mut connection, &format!("SELECT id, v FROM t WHERE {predicate}"));
    }
}

/// The keys of the small side of a join find the rows of the large one in its index, and the join
/// has to come out as it does without.
#[test]
fn joins_find_the_same_through_an_index() {
    let db = Database::open_in_memory().unwrap();
    let mut connection = db.connect().unwrap();
    for sql in [
        "CREATE TABLE big AS SELECT i AS id, i % 7 AS v, 'row ' || (i % 5) AS s FROM range(50000) r(i)",
        "CREATE INDEX big_id ON big (id)",
        "CREATE INDEX big_s ON big (s)",
        "INSERT INTO big VALUES (NULL, 0, 'row 0'), (5, 99, NULL), (5, 98, NULL)",
        "DELETE FROM big WHERE id % 11 = 0",
        "CREATE TABLE small AS SELECT i * 37 AS id, 'row ' || (i % 9) AS s FROM range(30) r(i)",
        "INSERT INTO small VALUES (NULL, NULL), (5, 'row 1'), (5, 'row 1'), (-1, 'none')",
        "CREATE TABLE narrow AS SELECT (i * 37)::INTEGER AS id FROM range(30) r(i)",
    ] {
        run(&mut connection, sql);
    }
    let queries = [
        "SELECT b.id, b.v FROM big b JOIN small s ON b.id = s.id",
        "SELECT b.id, b.v, s.s FROM small s JOIN big b ON s.id = b.id WHERE b.v < 3",
        "SELECT b.id FROM big b SEMI JOIN small s USING (id)",
        "SELECT b.id FROM big b ANTI JOIN small s USING (id) WHERE b.id < 100",
        "SELECT s.id, b.v FROM small s LEFT JOIN big b ON s.id = b.id",
        "SELECT b.id, s.id FROM big b LEFT JOIN small s ON s.id = b.id WHERE b.id < 200",
        "SELECT b.id FROM big b WHERE b.id IN (SELECT id FROM small)",
        "SELECT b.id FROM big b WHERE b.id NOT IN (SELECT id FROM small WHERE id IS NOT NULL) AND b.id < 200",
        "SELECT b.id FROM big b JOIN (SELECT DISTINCT id FROM small) s ON b.id = s.id",
        "SELECT b.id FROM big b JOIN small s ON b.id = s.id AND b.v > s.id % 3",
        "SELECT b.id FROM big b JOIN narrow n ON b.id = n.id",
        "SELECT b.v, s.s FROM big b JOIN small s ON b.s = s.s",
        "SELECT b.id FROM big b JOIN small s ON b.id = s.id + 1",
        "SELECT b.id FROM big b JOIN (SELECT id FROM small LIMIT 3) s ON b.id = s.id",
    ];
    let both = |connection: &mut Connection, step: &str| {
        for sql in queries {
            run(connection, "SET dynamic_or_filter_threshold = 50");
            let through_index = rows(connection, sql);
            run(connection, "SET dynamic_or_filter_threshold = 0");
            assert_eq!(through_index, rows(connection, sql), "{step}: {sql}");
        }
        run(connection, "SET dynamic_or_filter_threshold = 50");
    };
    both(&mut connection, "at first");
    assert!(used_index(&mut connection, queries[0]));
    run(&mut connection, "BEGIN");
    run(&mut connection, "DELETE FROM big WHERE id = 37");
    run(&mut connection, "INSERT INTO big VALUES (74, -1, 'new')");
    run(&mut connection, "INSERT INTO small VALUES (42, 'x')");
    both(&mut connection, "in a transaction");
    run(&mut connection, "ROLLBACK");
    both(&mut connection, "after a rollback");
}
