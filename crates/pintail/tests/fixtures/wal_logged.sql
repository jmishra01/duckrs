PRAGMA disable_checkpoint_on_shutdown;

SET checkpoint_threshold = '100GB';

INSERT INTO kept VALUES (10000, 'inserted'), (10001, NULL);

DELETE FROM kept WHERE i BETWEEN 100 AND 200;

UPDATE kept SET s = 'updated' WHERE i % 1000 = 1;

UPDATE kept SET i = -i, s = NULL WHERE i = 3;

DELETE FROM kept WHERE i = 10000;

BEGIN;

INSERT INTO kept VALUES (20000, 'rolled back');

ROLLBACK;

DROP TABLE doomed;

DROP VIEW doomed_view;

DROP MACRO doomed_macro;

DROP SEQUENCE doomed_sequence;

CREATE SCHEMA logged_schema;

CREATE TABLE logged_schema.fresh (i INTEGER DEFAULT 5, b BOOLEAN, d DECIMAL(9,2), h HUGEINT, m mood, t TIMESTAMP, bl BLOB, l INTEGER[], st STRUCT(a INTEGER, b VARCHAR), mp MAP(VARCHAR, INTEGER), ar INTEGER[2]);

INSERT INTO logged_schema.fresh VALUES (1, true, 1.5, 170141183460469231731687303715884105727, 'b', TIMESTAMP '2020-01-02 03:04:05', 'blob', [1, NULL, 3], {'a': 1, 'b': 'x'}, MAP {'k': 1}, [1, 2]), (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), (3, false, -2.25, -1, 'c', TIMESTAMP '1999-12-31 23:59:59', '', [], {'a': NULL, 'b': NULL}, MAP {}, [NULL, 4]);

CREATE TABLE constants AS SELECT 7 AS seven, 'same' AS same, i FROM range(3000) r(i);

CREATE TABLE repeated AS SELECT 'value ' || (i % 3) AS s FROM range(5000) r(i);

CREATE TABLE bulk (i BIGINT, s VARCHAR);

INSERT INTO bulk SELECT i, 'bulk ' || (i % 10) FROM range(300000) r(i);

DELETE FROM bulk WHERE i % 50000 = 1;

UPDATE bulk SET s = 'after bulk' WHERE i = 299999;

INSERT INTO bulk VALUES (-1, 'after the row groups');

CREATE VIEW logged_view AS SELECT count(*) AS c FROM bulk;

CREATE MACRO logged_macro(x) AS x * 3;

CREATE MACRO logged_table_macro() AS TABLE SELECT 1 AS one;

CREATE TYPE logged_type AS ENUM ('x', 'y');

CREATE INDEX logged_index ON kept (i);

SELECT nextval('counter');

SELECT nextval('counter');
