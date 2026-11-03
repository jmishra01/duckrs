//! The transformer and DuckDB-style SQL rendering (`ToString`).

fn render(sql: &str) -> String {
    let statements = duckrs_parser::parse_ast(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    statements.iter().map(|s| s.to_string()).collect::<Vec<_>>().join("; ")
}

fn error(sql: &str) -> String {
    duckrs_parser::parse_ast(sql).expect_err(sql).to_string()
}

#[test]
fn renders_expressions_like_duckdb() {
    let cases = [
        (
            "SELECT a+b*c, -x, -1, NOT a AND b OR c",
            "SELECT (a + (b * c)), (-(x)), -1, (((NOT a) AND b) OR c)",
        ),
        (
            "SELECT a IS NULL, a IS NOT TRUE, a BETWEEN 1 AND 2, a NOT IN (1,2), a LIKE 'x%', a NOT LIKE 'y'",
            "SELECT (a IS NULL), (CAST(a AS BOOLEAN) IS DISTINCT FROM true), (a BETWEEN 1 AND 2), \
             (NOT (a IN (1, 2))), (a ~~ 'x%'), (a !~~ 'y')",
        ),
        (
            "SELECT [1,2], {'a':1}, MAP {1: 2}, a.b.c, x[1], x[1:2], (1, 2), row(1)",
            "SELECT list_value(1, 2), struct_pack(a := 1), \"map\"(list_value(1), list_value(2)), a.b.c, x[1], \
             x[1:2], \"row\"(1, 2), \"row\"(1)",
        ),
        (
            "SELECT f(DISTINCT x ORDER BY y) FILTER (WHERE z), count(*), ifnull(a, b), x.lower()",
            "SELECT f(DISTINCT x ORDER BY y) FILTER (WHERE z), count_star(), COALESCE(a, b), x.lower()",
        ),
        (
            "SELECT sum(x) OVER (PARTITION BY a ORDER BY b ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
             first(x) OVER ()",
            "SELECT sum(x) OVER (PARTITION BY a ORDER BY b ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), \
             first_value(x) OVER ()",
        ),
        (
            "SELECT CASE WHEN a THEN 1 ELSE 2 END, CASE x WHEN 1, 2 THEN 3 END",
            "SELECT CASE  WHEN (a) THEN (1) ELSE 2 END, CASE  WHEN (((x = 1) OR (x = 2))) THEN (3) ELSE NULL END",
        ),
        (
            "SELECT CAST(1 AS int4), 'a'::varchar(10), x::decimal(10,2), y::int[], z::struct(a int, b varchar)",
            "SELECT CAST(1 AS int4), CAST('a' AS VARCHAR(10)), CAST(x AS DECIMAL(10, 2)), CAST(y AS INTEGER[]), \
             CAST(z AS STRUCT(a INTEGER, b VARCHAR))",
        ),
        (
            "SELECT INTERVAL '1' DAY, INTERVAL 2 HOURS, DATE '1992-01-01', date('x')",
            "SELECT to_days(CAST(trunc(CAST('1' AS DOUBLE)) AS INTEGER)), to_hours(CAST(trunc(CAST(2 AS DOUBLE)) \
             AS BIGINT)), CAST('1992-01-01' AS DATE), CAST('x' AS DATE)",
        ),
        (
            "SELECT trim(both 'x' from y), substring('abc' from 2 for 1), extract(year from d), position('a' in 'b')",
            "SELECT \"trim\"(y, 'x'), \"substring\"('abc', 2, 1), date_part('YEAR', d), \"position\"('b', 'a')",
        ),
        (
            "SELECT x -> x + 1, lambda y: y * 2, [x for x in l if x > 1]",
            "SELECT (x -> (x + 1)), (lambda y: (y * 2)), list_apply(list_filter(list_apply(l, (lambda x: \
             struct_pack(\"filter\" := (x > 1), result := x))), (lambda elem: struct_extract(elem, 'filter'))), \
             (lambda elem: struct_extract(elem, 'result')))",
        ),
        (
            "SELECT E'a\\tb', X'FF', B'101', N'x', $1, ?, $name, #1, 'it''s'",
            "SELECT 'a\tb', X'FF', B'101', CAST('x' AS VARCHAR), $1, $2, $name, #1, 'it''s'",
        ),
        (
            "SELECT COLUMNS('a.*'), * EXCLUDE (a) REPLACE (b + 1 AS b), t.*, \"Quoted\", \"select\"",
            "SELECT COLUMNS('a.*'), * EXCLUDE (a) REPLACE ((b + 1) AS b), t.*, Quoted, \"select\"",
        ),
        (
            "SELECT EXISTS(SELECT 1), NOT EXISTS(SELECT 1), (SELECT 1), x IN (SELECT 1), x = ANY([1])",
            "SELECT EXISTS(SELECT 1), (NOT EXISTS(SELECT 1)), (SELECT 1), (x = ANY(SELECT 1)), \
             (x = ANY(SELECT UNNEST(list_value(1))))",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
}

#[test]
fn renders_queries_like_duckdb() {
    let cases = [
        (
            "SELECT * EXCLUDE (a) FROM t AS u(x) JOIN v USING (a) WHERE a > 1 GROUP BY a HAVING b \
             ORDER BY 1 DESC NULLS LAST LIMIT 10 OFFSET 2",
            "SELECT * EXCLUDE (a) FROM (t AS u(x) INNER JOIN v USING (a)) WHERE (a > 1) GROUP BY a HAVING b \
             ORDER BY 1 DESC NULLS LAST LIMIT 10 OFFSET 2",
        ),
        (
            "WITH RECURSIVE r AS (SELECT 1 AS x UNION ALL SELECT x + 1 FROM r WHERE x < 10) SELECT * FROM r",
            "WITH RECURSIVE r AS ((SELECT 1 AS x) UNION ALL (SELECT (x + 1) FROM r WHERE (x < 10))) SELECT * FROM r",
        ),
        (
            "SELECT 1 UNION SELECT 2 UNION SELECT 3 INTERSECT SELECT 4",
            "(SELECT 1) UNION (SELECT 2)UNION ((SELECT 3) INTERSECT (SELECT 4))",
        ),
        ("FROM tbl", "SELECT * FROM tbl"),
        (
            "VALUES (1, 'a'), (2, 'b')",
            "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS valueslist",
        ),
        (
            "SELECT a, count(*) FROM t GROUP BY CUBE (a, b)",
            "SELECT a, count_star() FROM t GROUP BY GROUPING SETS ((),(a),(a, b),(b))",
        ),
        ("SELECT a FROM t GROUP BY ALL", "SELECT a FROM t GROUP BY ALL"),
        (
            "SELECT * FROM range(10) t(i), s.f(x := 1) WITH ORDINALITY CROSS JOIN u NATURAL LEFT JOIN v",
            "SELECT * FROM \"range\"(10) AS t(i) , ((s.f(x := 1) WITH ORDINALITY CROSS JOIN u) NATURAL LEFT JOIN v)",
        ),
        (
            "SELECT row_number() OVER w FROM t WINDOW w AS (PARTITION BY a ORDER BY b)",
            "SELECT row_number() OVER (PARTITION BY a ORDER BY b) FROM t",
        ),
        (
            "SELECT DISTINCT ON (a) a, b FROM t",
            "SELECT DISTINCT ON (a) a, b FROM t",
        ),
        (
            "SELECT * FROM t USING SAMPLE 10% (bernoulli, 5)",
            "SELECT * FROM t USING SAMPLE 10.0% (Bernoulli, 5)",
        ),
        ("SELECT 1 LIMIT 10%", "SELECT 1 LIMIT (10) %"),
        ("TABLE t", "SELECT * FROM t"),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
}

#[test]
fn renderings_parse_back_to_themselves() {
    let queries = [
        "SELECT a.\"b c\", \"select\".x, 1.5e3, 'x' COLLATE nocase, ts AT TIME ZONE 'UTC' FROM s.t",
        "SELECT CAST(x AS MAP(VARCHAR, INTEGER)[3]), CAST(y AS UNION(a INTEGER, b VARCHAR))",
        "SELECT lag(x, 1 IGNORE NULLS) OVER (ORDER BY y RANGE BETWEEN UNBOUNDED PRECEDING AND 2 FOLLOWING EXCLUDE TIES)",
        "SELECT * FROM a ASOF JOIN b ON a.t >= b.t POSITIONAL JOIN c",
        "SELECT quantile_cont(x ORDER BY y), mode() WITHIN GROUP (ORDER BY z) FROM t",
        "SELECT CAST(t AS TIMESTAMP WITH TIME ZONE), CAST(t AS TIMESTAMP(3))",
    ];
    for sql in queries {
        let once = render(sql);
        assert_eq!(render(&once), once, "{sql}");
    }
}

#[test]
fn transformer_errors_match_duckdb() {
    let cases = [
        (
            "SELECT 1 < 2 < 3",
            "Parser Error: Chained comparisons are not supported, use AND to combine comparisons",
        ),
        (
            "SELECT * EXCLUDE (a, a) FROM t",
            "Parser Error: Duplicate entry \"a\" in EXCLUDE list",
        ),
        (
            "SELECT x::INT[1:2]",
            "Parser Error: Subscript/slice cannot be applied directly after a cast operator",
        ),
        (
            "SELECT INTERVAL '1' YEAR TO MONTH",
            "Parser Error: YEAR TO MONTH is not supported",
        ),
        (
            "VALUES (1), (1, 2)",
            "Parser Error: VALUES lists must all be the same length",
        ),
        ("SELECT f(x) OVER w", "Parser Error: window \"w\" does not exist"),
        (
            "SELECT count(DISTINCT)",
            "Parser Error: DISTINCT requires at least one argument",
        ),
        ("SELECT #0", "Parser Error: Positional reference node needs to be >= 1"),
        (
            "ALTER TABLE t ADD COLUMN c INTEGER",
            "Not implemented Error: the transformer for AlterStatement",
        ),
    ];
    for (sql, expected) in cases {
        let err = error(sql);
        assert!(err.starts_with(expected), "{sql}: {err}");
    }
}

#[test]
fn renders_statements_like_duckdb() {
    let cases = [
        (
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            "INSERT INTO t (VALUES (1, 'a'), (2, 'b'))",
        ),
        (
            "INSERT OR REPLACE INTO main.t AS x BY NAME (a, b) SELECT 1, 2 RETURNING a AS z",
            "INSERT OR REPLACE INTO main.t AS x BY NAME (a, b) SELECT 1, 2 RETURNING a AS z",
        ),
        (
            "INSERT INTO t AS x DEFAULT VALUES ON CONFLICT (A) WHERE a > 1 DO UPDATE SET B = 1 WHERE b < 2",
            "INSERT INTO t AS x DEFAULT VALUES ON CONFLICT (a ) WHERE (a > 1) DO UPDATE SET b = 1 WHERE (b < 2)",
        ),
        (
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
            "INSERT INTO t (VALUES (1)) ON CONFLICT  DO NOTHING",
        ),
        (
            "UPDATE t AS u SET a = 1, (b, c) = (2, 3) FROM s WHERE u.id = s.id RETURNING *",
            "UPDATE t AS u SET a = 1, b = 2, c = 3 FROM s WHERE (u.id = s.id) RETURNING *",
        ),
        (
            "DELETE FROM t AS x USING s, r WHERE x.a = s.a",
            "DELETE FROM t AS x USING s, r WHERE (x.a = s.a)",
        ),
        ("TRUNCATE TABLE t", "DELETE FROM t"),
        (
            "CREATE TABLE t (i INTEGER PRIMARY KEY, s VARCHAR NOT NULL COLLATE nocase, d DECIMAL(10, 2) UNIQUE, \
             g AS (i + 1), CHECK (i > 0), FOREIGN KEY (i) REFERENCES p(id), UNIQUE (s, d))",
            "CREATE TABLE t(i INTEGER PRIMARY KEY, s VARCHAR('nocase') NOT NULL, d DECIMAL(10, 2) UNIQUE, \
             g  GENERATED ALWAYS AS((i + 1)), CHECK((i > 0)), FOREIGN KEY (i) REFERENCES p(id), UNIQUE(s, d));",
        ),
        (
            "CREATE OR REPLACE TEMP TABLE s.t AS SELECT 1 WITH NO DATA",
            "CREATE OR REPLACE TEMP TABLE s.t AS SELECT 1 LIMIT 0",
        ),
        (
            "CREATE TABLE IF NOT EXISTS t2 (a, b) AS SELECT 1",
            "CREATE TABLE  IF NOT EXISTS t2(a , b ) AS SELECT 1",
        ),
        (
            "CREATE TABLE t (i INT DEFAULT 3 USING COMPRESSION rle) WITH (foo = 1)",
            "CREATE TABLE t(i INTEGER DEFAULT(3) USING COMPRESSION RLE) WITH ('foo'=1);",
        ),
        ("DROP TABLE IF EXISTS main.t CASCADE", "DROP TABLE IF EXISTS t CASCADE;"),
        ("DROP FUNCTION m", "DROP MACRO m;"),
        ("BEGIN TRANSACTION READ ONLY; END", "BEGIN READ ONLY;; COMMIT;"),
        ("FORCE CHECKPOINT db", "CALL \"system\".main.force_checkpoint('db');"),
        ("CALL range(1, 2)", "CALL \"range\"(1, 2);"),
        ("SET threads = 4", "SET  threads TO 4;"),
        ("SET GLOBAL x TO y", "SET GLOBAL x TO 'y';"),
        ("SET threads = DEFAULT", "RESET  threads;"),
        ("SET TIME ZONE 'UTC'", "SET  timezone TO 'UTC';"),
        ("PRAGMA table_info('t')", "PRAGMA table_info('t');"),
        ("PRAGMA threads = 4", "SET  threads TO 4;"),
        ("EXPLAIN ANALYZE SELECT 1", "EXPLAIN (ANALYZE) SELECT 1"),
        (
            "EXPLAIN (FORMAT json) DELETE FROM t",
            "EXPLAIN (FORMAT JSON) DELETE FROM t",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
}

#[test]
fn statement_errors_match_duckdb() {
    let cases = [
        (
            "CREATE TABLE t ()",
            "Parser Error: Table must have at least one column!",
        ),
        (
            "CREATE TABLE t (a)",
            "Parser Error: Column a must have a type or be defined as a GENERATED column.",
        ),
        (
            "CREATE TABLE t (a INT DEFAULT 1 DEFAULT 2)",
            "Parser Error: Cannot define a default value twice",
        ),
        (
            "CREATE TABLE t (a INT COLLATE x)",
            "Parser Error: Only VARCHAR columns can have collations!",
        ),
        (
            "CREATE TABLE t (a INT CHECK (a IN (SELECT 1)))",
            "Parser Error: subqueries prohibited in CHECK constraints",
        ),
        (
            "CREATE TABLE t AS INSERT INTO s VALUES (1)",
            "Parser Error: CREATE TABLE AS requires a SELECT clause",
        ),
        (
            "DROP TABLE a, b",
            "Not implemented Error: Can only drop one object at a time",
        ),
        (
            "INSERT INTO t (a) DEFAULT VALUES",
            "Parser Error: You can not provide both a column list and DEFAULT VALUES",
        ),
        (
            "UPDATE t SET (a, b) = (1, 2, 3)",
            "Parser Error: Could not perform assignment, expected 2 values, got 3",
        ),
        (
            "SET threads = 1, 2",
            "Parser Error: SET can only contain a single value",
        ),
        (
            "SET LOCAL threads = 1",
            "Not implemented Error: SET LOCAL is not implemented.",
        ),
        (
            "EXPLAIN (SQL, ANALYZE) SELECT 1",
            "Invalid Input Error: EXPLAIN (SQL) cannot be combined with ANALYZE",
        ),
    ];
    for (sql, expected) in cases {
        let err = error(sql);
        assert!(err.starts_with(expected), "{sql}: {err}");
    }
}

#[test]
fn renders_create_and_session_statements_like_duckdb() {
    let cases = [
        (
            "CREATE VIEW v (a, b) AS SELECT 1, 2",
            "CREATE VIEW v (a, b) AS SELECT 1, 2;",
        ),
        (
            "CREATE OR REPLACE TEMP VIEW s.v WITH (defer_binding = true) AS SELECT 1",
            "CREATE OR REPLACE TEMP VIEW s.v WITH (DEFER_BINDING) AS SELECT 1;",
        ),
        (
            "CREATE RECURSIVE VIEW nums (n) AS SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < 5",
            "CREATE VIEW nums (n) AS WITH RECURSIVE nums (n) AS ((SELECT 1) UNION ALL (SELECT (n + 1) FROM nums \
             WHERE (n < 5))) SELECT n FROM nums;",
        ),
        ("CREATE SCHEMA IF NOT EXISTS db.s", "CREATE SCHEMA IF NOT EXISTS db.s;"),
        (
            "CREATE SEQUENCE seq INCREMENT BY -1 MINVALUE -10 CYCLE",
            "CREATE SEQUENCE seq INCREMENT BY -1 MINVALUE -10 MAXVALUE -1 START -1 CYCLE;",
        ),
        (
            "CREATE UNIQUE INDEX idx ON s.t (a, (b + 1)) WITH (x = 1)",
            "CREATE UNIQUE INDEX idx ON s.t(a, ((b + 1))) WITH (x = 1 );",
        ),
        (
            "CREATE INDEX i ON t USING HNSW (v)",
            "CREATE INDEX i ON t USING HNSW (v);",
        ),
        (
            "CREATE MACRO add(a, b := 1) AS a + b, (a INTEGER) AS a",
            "CREATE MACRO \"add\" (a, b := 1) AS ((a + b)), (a INTEGER) AS (a);",
        ),
        (
            "CREATE FUNCTION f(x) RETURNS INTEGER AS x * 2",
            "CREATE MACRO f (x) RETURNS INTEGER AS ((x * 2));",
        ),
        (
            "CREATE MACRO tm(n) AS TABLE SELECT * FROM range(n)",
            "CREATE MACRO tm (n) AS TABLE (SELECT * FROM \"range\"(n));",
        ),
        (
            "CREATE TYPE mood AS ENUM ('sad', 'happy')",
            "CREATE TYPE mood AS ENUM ( 'sad', 'happy' );",
        ),
        (
            "CREATE TYPE t2 AS STRUCT(a INT)",
            "CREATE TYPE t2 AS STRUCT(a INTEGER);",
        ),
        ("USE db.s", "SET  schema TO 'db.s';"),
        ("PREPARE p AS SELECT ?", "PREPARE p AS SELECT $1"),
        ("EXECUTE p(1, 'x')", "EXECUTE p(\"1\" := 1, \"2\" := 'x');"),
        ("DEALLOCATE p", "DEALLOCATE PREPARE p;"),
        ("DETACH IF EXISTS db", "DETACH DATABASE IF EXISTS db;"),
        (
            "WITH ins AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM ins",
            "WITH ins AS (INSERT INTO t (VALUES (1)) RETURNING *) SELECT * FROM ins",
        ),
        ("EXPLAIN (ANALYSE) SELECT 1", "EXPLAIN (ANALYZE) SELECT 1"),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
    let errors = [
        (
            "CREATE RECURSIVE VIEW v AS SELECT 1",
            "Parser Error: CREATE RECURSIVE VIEW requires a column list",
        ),
        (
            "CREATE SEQUENCE s INCREMENT 0",
            "Parser Error: Increment must not be zero",
        ),
        (
            "CREATE SEQUENCE s START 1 START 2",
            "Parser Error: Start should be passed at most once",
        ),
        (
            "CREATE SEQUENCE s MINVALUE 5 MAXVALUE 1",
            "Parser Error: MINVALUE (5) must be less than MAXVALUE (1)",
        ),
        (
            "CREATE INDEX ON t (a)",
            "Not implemented Error: Please provide an index name",
        ),
        (
            "CREATE TEMP INDEX i ON t (a)",
            "Parser Error: Temporary indexes are not supported",
        ),
        (
            "CREATE MACRO m(a := 1, b) AS a",
            "Parser Error: Parameter without a default follows",
        ),
        (
            "CREATE MACRO m(a, a) AS a",
            "Parser Error: Duplicate parameter a in macro definition",
        ),
        (
            "PREPARE p AS DROP TABLE t",
            "Parser Error: DROP_STATEMENT is not a preparable statement",
        ),
        ("EXECUTE p(a)", "Invalid Input Error: Only scalar parameters"),
        (
            "WITH RECURSIVE x AS (DELETE FROM t) SELECT 1",
            "Parser Error: Recursive CTEs with DML statements are not supported",
        ),
    ];
    for (sql, expected) in errors {
        let err = error(sql);
        assert!(err.starts_with(expected), "{sql}: {err}");
    }
}
