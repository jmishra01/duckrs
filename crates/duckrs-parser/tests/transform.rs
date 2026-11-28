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
            "SELECT E'a\\tb', X'FF', B'101', N'x', $1, ?, #1, 'it''s'",
            "SELECT 'a\tb', X'FF', B'101', CAST('x' AS VARCHAR), $1, $2, #1, 'it''s'",
        ),
        ("SELECT $name, $other", "SELECT $name, $other"),
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
            "INSERT INTO t AS x DEFAULT VALUES ON CONFLICT (A) WHERE (a > 1) DO UPDATE SET B = 1 WHERE (b < 2)",
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

#[test]
fn renders_alter_comment_and_describe_like_duckdb() {
    let cases = [
        (
            "ALTER TABLE IF EXISTS main.t RENAME COLUMN a TO b",
            "ALTER TABLE IF EXISTS t RENAME COLUMN a TO b;",
        ),
        (
            "ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 1",
            "ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 1;",
        ),
        (
            "ALTER TABLE t ADD COLUMN c INTEGER NOT NULL UNIQUE",
            "ALTER TABLE t ADD COLUMN c INTEGER;;ALTER TABLE t ALTER COLUMN c SET NOT NULL;;\
             ALTER TABLE t ADD UNIQUE(c);;",
        ),
        (
            "ALTER TABLE t ADD COLUMN ts TIMESTAMP DEFAULT now()",
            "ALTER TABLE t ADD COLUMN ts TIMESTAMP DEFAULT NULL;;UPDATE t SET ts = now();\
             ALTER TABLE t ALTER COLUMN ts SET DEFAULT now();;",
        ),
        (
            "ALTER TABLE t ADD COLUMN s.f VARCHAR",
            "ALTER TABLE t ADD COLUMN s.f VARCHAR;",
        ),
        (
            "ALTER TABLE t DROP COLUMN IF EXISTS c CASCADE",
            "ALTER TABLE t DROP COLUMN IF EXISTS c CASCADE;",
        ),
        (
            "ALTER TABLE t ALTER c TYPE BIGINT USING c + 1",
            "ALTER TABLE t ALTER COLUMN c TYPE BIGINT USING (c + 1);",
        ),
        (
            "ALTER TABLE t ALTER c DROP DEFAULT",
            "ALTER TABLE t ALTER COLUMN c DROP DEFAULT;",
        ),
        (
            "ALTER TABLE t ALTER c SET NOT NULL",
            "ALTER TABLE t ALTER COLUMN c SET NOT NULL;",
        ),
        (
            "ALTER TABLE t ADD PRIMARY KEY (a, b)",
            "ALTER TABLE t ADD PRIMARY KEY(a, b);",
        ),
        (
            "ALTER TABLE t SET SORTED BY (a DESC)",
            "ALTER TABLE t SET SORTED BY (a DESC)",
        ),
        ("ALTER TABLE t RESET (x)", "ALTER TABLE t RESET ('x')"),
        ("ALTER VIEW v RENAME TO w", "ALTER VIEW v RENAME TO w;"),
        ("ALTER SEQUENCE seq OWNED BY s.t", "ALTER SEQUENCE seq OWNED BY s.t;"),
        (
            "ALTER DATABASE db SET ALIAS TO db2",
            "ALTER DATABASE db SET ALIAS TO db2",
        ),
        ("COMMENT ON TABLE t IS 'hello'", "COMMENT ON TABLE t IS 'hello';"),
        ("COMMENT ON COLUMN t.c IS NULL", "COMMENT ON COLUMN t.c IS NULL;"),
        ("DESCRIBE t", "DESCRIBE (SELECT * FROM t)"),
        ("SHOW TABLES", "DESCRIBE \"tables\""),
        ("SHOW ALL TABLES", "DESCRIBE "),
        ("SHOW TABLES FROM db.s", "SHOW TABLES FROM db.s"),
        ("SUMMARIZE SELECT 1", "SUMMARIZE (SELECT 1)"),
        ("SHOW threads", "SHOW threads"),
        (
            "SELECT * FROM (DESCRIBE t)",
            "SELECT * FROM (DESCRIBE (SELECT * FROM t))",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
    let errors = [
        (
            "ALTER TABLE t RENAME TO a, RENAME TO b",
            "Parser Error: Only one ALTER command per statement",
        ),
        (
            "ALTER TABLE t ADD COLUMN c INT PRIMARY KEY",
            "Parser Error: Adding columns with PRIMARY KEY",
        ),
        (
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS c INT NOT NULL",
            "Not implemented Error: Adding a NOT NULL",
        ),
        (
            "ALTER TABLE t ALTER c TYPE",
            "Parser Error: Omitting the type is only possible in combination with USING",
        ),
        (
            "ALTER TABLE t RESET (x = 1)",
            "Parser Error: Reset option \"x\" cannot set any value",
        ),
        (
            "COMMENT ON COLUMN c IS 'x'",
            "Parser Error: Invalid column reference: c",
        ),
        ("SUMMARIZE", "Parser Error: Expected table name with SUMMARIZE"),
        (
            "DESCRIBE t ORDER BY 1",
            "Parser Error: DESCRIBE with ORDER BY not allowed",
        ),
    ];
    for (sql, expected) in errors {
        let err = error(sql);
        assert!(err.starts_with(expected), "{sql}: {err}");
    }
}

#[test]
fn renders_utility_statements_like_duckdb() {
    let cases = [
        (
            "COPY t TO 'out.csv' (HEADER, DELIMITER ',')",
            "COPY t TO  'out.csv' (header, delimiter ',' );",
        ),
        (
            "COPY t FROM 'in.parquet' (FORMAT parquet)",
            "COPY t FROM 'in.parquet' ( FORMAT 'parquet' );",
        ),
        (
            "COPY (SELECT 1) TO 'x.csv' WITH (FORMAT 'csv', PARTITION_BY (a, b), COMPRESSION zstd)",
            "COPY (SELECT 1) TO  'x.csv' ( FORMAT 'csv', partition_by \"row\"(a, b), compression 'zstd' );",
        ),
        (
            "COPY t (a, b) FROM stdin CSV HEADER DELIMITER '|' NULL 'NA'",
            "COPY t (a, b ) FROM '/dev/stdin' ( FORMAT 'csv', header true, null 'NA', delimiter '|' );",
        ),
        (
            "COPY t TO 'o.csv' (ORDER BY (a DESC, b))",
            "COPY t TO  'o.csv' (order_by \"row\"('a DESC', 'b') );",
        ),
        ("COPY t TO $1", "COPY t TO ($1);"),
        ("COPY FROM DATABASE a TO b", "PRAGMA copy_database('a', 'b');"),
        (
            "COPY FROM DATABASE a TO b (SCHEMA)",
            "COPY FROM DATABASE a TO b (SCHEMA)",
        ),
        (
            "EXPORT DATABASE 'dir' (FORMAT parquet, COMPRESSION 'zstd')",
            "EXPORT DATABASE 'dir' ( FORMAT 'parquet', compression 'zstd' );",
        ),
        ("IMPORT DATABASE 'dir'", "PRAGMA import_database('dir');"),
        (
            "ATTACH 'file.db' AS f (READ_ONLY, TYPE sqlite)",
            "ATTACH DATABASE 'file.db' AS f (read_only true, \"type\" 'sqlite');",
        ),
        (
            "ATTACH IF NOT EXISTS ':memory:'",
            "ATTACH IF NOT EXISTS DATABASE ':memory:';",
        ),
        ("ANALYZE t(a, b)", "VACUUM ANALYZE t(a, b);"),
        ("VACUUM (ANALYZE) t", "VACUUM ANALYZE t;"),
        ("INSTALL 'spatial' FROM community", "INSTALL 'spatial' FROM community;"),
        (
            "FORCE INSTALL AND LOAD x FROM 'https://r'",
            "FORCE INSTALL AND LOAD 'x' FROM 'https://r';",
        ),
        ("LOAD x AS y", "LOAD 'x' AS y;"),
        (
            "CREATE EXTENSION REPOSITORY r WITH PREFIX 'http://x' USING PUBLIC KEY 'k'",
            "CREATE EXTENSION REPOSITORY r WITH PREFIX 'http://x' USING PUBLIC KEY 'k';",
        ),
        (
            "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW INSERT INTO log VALUES (1)",
            "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW INSERT INTO log (VALUES (1));",
        ),
        (
            "CREATE TRIGGER trg BEFORE UPDATE OF a ON t REFERENCING NEW TABLE AS n DELETE FROM x",
            "CREATE TRIGGER trg BEFORE UPDATE OF a ON t REFERENCING NEW TABLE AS n FOR EACH STATEMENT DELETE FROM x;",
        ),
        (
            "CREATE SECRET (TYPE s3, KEY_ID 'abc')",
            "CREATE SECRET  __default_s3(TYPE 's3', key_id 'abc')",
        ),
        ("CREATE TEMP SECRET s2 (TYPE gcs)", "CREATE TEMP SECRET  s2(TYPE 'gcs')"),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
    let errors = [
        (
            "COPY t TO 'x' (A 1, a 2)",
            "Parser Error: Unexpected duplicate option \"a\"",
        ),
        (
            "COPY t TO 'x' (FORMAT)",
            "Parser Error: Unsupported parameter type for FORMAT",
        ),
        (
            "COPY t TO 'x' (DELIMITER (a DESC))",
            "Parser Error: ORDER BY modifiers are only supported",
        ),
        (
            "ATTACH OR REPLACE IF NOT EXISTS 'x'",
            "Parser Error: Cannot specify both OR REPLACE and IF NOT EXISTS",
        ),
        (
            "ATTACH 'x' (TYPE NULL)",
            "Binder Error: NULL is not supported as a valid option for ATTACH option \"type\"",
        ),
        ("VACUUM FULL", "Not implemented Error: FULL is not yet implemented"),
        ("VACUUM (FOO)", "Parser Error: unrecognized VACUUM option \"FOO\""),
        (
            "CREATE SECRET (KEY_ID 'a')",
            "Parser Error: Failed to create secret - secret must have a type defined",
        ),
        (
            "CREATE TEMP TRIGGER t AFTER INSERT ON x DELETE FROM y",
            "Parser Error: Temporary triggers are not supported",
        ),
        (
            "CREATE PERSISTENT TABLE t (a INT)",
            "Parser Error: PERSISTENT is only supported for CREATE SECRET",
        ),
    ];
    for (sql, expected) in errors {
        let err = error(sql);
        assert!(err.starts_with(expected), "{sql}: {err}");
    }
}

#[test]
fn renders_pivot_merge_and_nearest_like_duckdb() {
    let cases = [
        (
            "PIVOT cities ON year IN (2000, 2010) USING sum(population) AS total GROUP BY country",
            "SELECT * FROM cities PIVOT (sum(population) AS total FOR (\"year\") IN (2000, 2010) GROUP BY country)",
        ),
        (
            "PIVOT cities ON country IN (NL, US)",
            "SELECT * FROM cities PIVOT (count_star() FOR (country) IN ('NL', 'US'))",
        ),
        (
            "PIVOT cities USING sum(population) GROUP BY country",
            "SELECT country, sum(population) FROM cities GROUP BY country",
        ),
        (
            "UNPIVOT monthly ON jan, feb INTO NAME month VALUE sales",
            "SELECT * FROM monthly UNPIVOT (sales FOR \"month\" IN (jan, feb))",
        ),
        (
            "SELECT * FROM cities PIVOT (sum(population) FOR year IN (2000, 2010) GROUP BY country) AS p",
            "SELECT * FROM cities PIVOT (sum(population) FOR (\"year\") IN (2000, 2010) GROUP BY country) AS p",
        ),
        (
            "SELECT * FROM monthly UNPIVOT INCLUDE NULLS (sales FOR month IN (jan, feb))",
            "SELECT * FROM monthly UNPIVOT INCLUDE NULLS (sales FOR \"month\" IN (jan, feb))",
        ),
        (
            "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id) \
             WHEN MATCHED AND s.x > 1 THEN UPDATE SET v = s.v WHEN MATCHED THEN DELETE",
            "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED  AND (s.x > 1) THEN UPDATE SET v = s.v \
             WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.id)",
        ),
        (
            "MERGE INTO t AS x USING s USING (id) WHEN NOT MATCHED BY SOURCE THEN DO NOTHING \
             WHEN NOT MATCHED THEN INSERT BY NAME RETURNING *",
            "MERGE INTO t AS x USING s USING (id) WHEN NOT MATCHED BY SOURCE THEN DO NOTHING \
             WHEN NOT MATCHED THEN INSERT BY NAME * RETURNING *",
        ),
        (
            "SELECT * FROM a JOIN b NEAREST 3 BY DISTANCE a.v <-> b.v",
            "SELECT * FROM (a INNER JOIN b EXACT NEAREST 3 BY DISTANCE (a.v <-> b.v))",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
    // A pivot whose values come from the data first creates an ENUM of them.
    let rendered = render("PIVOT cities ON year USING sum(population)");
    assert!(
        rendered.starts_with("CREATE OR REPLACE TEMP TYPE \"__pivot_enum_")
            && rendered.contains("AS ENUM (SELECT DISTINCT CAST(\"year\" AS VARCHAR) FROM cities WHERE (\"year\" IS NOT NULL) ORDER BY 1 ASC);;SELECT * FROM cities PIVOT (sum(population) FOR (\"year\") IN \"__pivot_enum_"),
        "{rendered}"
    );
    let errors = [
        (
            "PIVOT t",
            "Parser Error: PIVOT requires an ON, USING or GROUP BY clause",
        ),
        ("PIVOT t ON 1", "Parser Error: Cannot pivot on constant value \"1\""),
        (
            "CREATE VIEW v AS PIVOT t ON a",
            "Parser Error: PIVOT statements with pivot elements extracted from the data cannot be used in views",
        ),
        (
            "PIVOT (SELECT ?) ON a",
            "Parser Error: PIVOT statements with pivot elements extracted from the data cannot have parameters",
        ),
        (
            "MERGE INTO t USING s USING (id) WHEN MATCHED THEN DELETE WHEN MATCHED THEN DELETE",
            "Parser Error: Unconditional WHEN MATCHED clause was already defined",
        ),
        (
            "SELECT * FROM a FULL JOIN b NEAREST BY DISTANCE x",
            "Parser Error: NEAREST BY only supports INNER and LEFT OUTER joins, not FULL",
        ),
        (
            "SELECT * FROM a JOIN b NEAREST 0 BY DISTANCE x",
            "Parser Error: NEAREST expects a positive integer literal, got \"0\"",
        ),
    ];
    for (sql, expected) in errors {
        let err = error(sql);
        assert!(err.starts_with(expected), "{sql}: {err}");
    }
}

#[test]
fn renders_match_recognize_and_remaining_statements_like_duckdb() {
    let cases = [
        (
            "SELECT * FROM t MATCH_RECOGNIZE (PARTITION BY a ORDER BY ts MEASURES FINAL count(*) AS n \
             ONE ROW PER MATCH AFTER MATCH SKIP PAST LAST ROW PATTERN (A B+? C{2,} (D | E)*) \
             SUBSET U = (A, B) DEFINE A AS x > 1) AS m",
            "SELECT * FROM t MATCH_RECOGNIZE(PARTITION BY a ORDER BY ts MEASURES __mr_final(count_star()) AS n \
             ONE ROW PER MATCH AFTER MATCH SKIP PAST LAST ROW PATTERN ((A{1,1} B+? C{2,} (D{1,1} | E{1,1})*)) \
             SUBSET U = (A, B) DEFINE A AS (x > 1)) AS m",
        ),
        (
            "SELECT * FROM t MATCH_RECOGNIZE (PATTERN (PERMUTE(A, B) ^ ()) DEFINE AUTO)",
            "SELECT * FROM t MATCH_RECOGNIZE(PATTERN ((((A{1,1} B{1,1}) | (B{1,1} A{1,1})){1,1} ^{1,1} (){1,1})) \
             DEFINE AUTO)",
        ),
        (
            "CREATE EXTERNAL RESOURCE 'kv' AS r (ttl 5)",
            "CREATE EXTERNAL RESOURCE 'kv' AS r (ttl 5)",
        ),
        // a query over `duckdb_external_resources` (`TransformShowExternalResources`)
        (
            "SHOW ALL EXTERNAL RESOURCES",
            "SELECT * FROM duckdb_external_resources(discover := true)",
        ),
        (
            "ATTACH TO EXTERNAL RESOURCE r AS db (READ_ONLY)",
            "ATTACH TO EXTERNAL RESOURCE r AS db (read_only true);",
        ),
        ("CONNECT 'db.duckdb'", "CONNECT 'db.duckdb';"),
        ("CONNECT LOCAL", "CONNECT LOCAL;"),
        ("DISCONNECT", "DISCONNECT;"),
        ("UPDATE EXTENSIONS (httpfs, json)", "UPDATE EXTENSIONS(httpfs, json)"),
        (
            "CREATE TYPE e AS ENUM ('a', 'b')[]",
            "CREATE TYPE e AS ENUM('a', 'b')[];",
        ),
        (
            "WITH c AS (COPY t TO 'x.csv') SELECT 1",
            "WITH c AS (COPY t TO  'x.csv') SELECT 1",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(render(sql), expected, "{sql}");
    }
    let errors = [
        (
            "SELECT * FROM t MATCH_RECOGNIZE (DEFINE A AS true)",
            "Parser Error: MATCH_RECOGNIZE requires a PATTERN clause",
        ),
        (
            "SELECT * FROM t MATCH_RECOGNIZE (PATTERN (A))",
            "Parser Error: MATCH_RECOGNIZE requires a DEFINE clause",
        ),
        (
            "SELECT * FROM t MATCH_RECOGNIZE (PATTERN (A) PATTERN (B) DEFINE A AS true)",
            "Parser Error: MATCH_RECOGNIZE has more than one PATTERN clause",
        ),
        (
            "SELECT * FROM t MATCH_RECOGNIZE (PATTERN (A{3,1}) DEFINE A AS true)",
            "Parser Error: Min count cannot be larger than max count",
        ),
        (
            "SELECT * FROM t MATCH_RECOGNIZE (PATTERN (A{0}) DEFINE A AS true)",
            "Parser Error: A pattern quantifier's upper bound must be at least 1",
        ),
        (
            "CONNECT TO NEW TEMPORARY EXTERNAL RESOURCE 'kv' (a 1) (b 2)",
            "Parser Error: CONNECT TO NEW TEMPORARY EXTERNAL RESOURCE",
        ),
        (
            "WITH c AS (COPY t FROM 'x.csv') SELECT 1",
            "Parser Error: COPY FROM cannot be used as a CTE body",
        ),
    ];
    for (sql, expected) in errors {
        let err = error(sql);
        assert!(err.starts_with(expected), "{sql}: {err}");
    }
}

#[test]
fn expression_depth_is_limited_like_duckdb() {
    let limit = "Parser Error: Max expression depth limit of 1000 exceeded";
    let nots = format!("SELECT {}1", "NOT ".repeat(1001));
    assert!(error(&nots).starts_with(limit), "{}", error(&nots));
    let sums = format!("SELECT 1{}", " + 1".repeat(1000));
    assert!(error(&sums).starts_with(limit));
    let lists = format!("SELECT {}1{}", "[".repeat(1001), "]".repeat(1001));
    assert!(error(&lists).starts_with(limit));
    assert!(duckrs_parser::parse_ast(&format!("SELECT 1{}", " + 1".repeat(500))).is_ok());

    let options = duckrs_parser::ParserOptions {
        max_expression_depth: 100,
        ..Default::default()
    };
    let casts = format!("SELECT 1{}", "::INTEGER".repeat(150));
    let err = duckrs_parser::parse_ast_with(&casts, &options).unwrap_err().to_string();
    assert!(
        err.starts_with("Parser Error: Max expression depth limit of 100 exceeded"),
        "{err}"
    );
    let subqueries = format!("SELECT {}1{}", "(SELECT ".repeat(101), ")".repeat(101));
    assert!(duckrs_parser::parse_ast_with(&subqueries, &options).is_err());
}
