CREATE SCHEMA kept_schema;

CREATE TYPE mood AS ENUM ('a', 'b', 'c');

CREATE SEQUENCE counter START 10 INCREMENT BY 2;

CREATE TABLE kept (i INTEGER, s VARCHAR);

INSERT INTO kept SELECT i, 'row ' || i FROM range(5000) r(i);

DELETE FROM kept WHERE i % 7 = 0;

CREATE TABLE doomed (i INTEGER);

INSERT INTO doomed VALUES (1);

CREATE VIEW doomed_view AS SELECT 1 AS one;

CREATE MACRO doomed_macro(x) AS x + 1;

CREATE SEQUENCE doomed_sequence;

SELECT nextval('counter');

CHECKPOINT;
