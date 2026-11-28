CREATE SCHEMA other_schema;

CREATE TYPE mood AS ENUM ('a', 'b', 'c');

CREATE SEQUENCE counter START 10 INCREMENT BY 2;

CREATE SEQUENCE other_schema.spent START 5;

SELECT nextval('other_schema.spent');

CREATE TABLE base (i INTEGER, s VARCHAR);

INSERT INTO base VALUES (1, 'row 1'), (2, 'row 2'), (3, NULL), (4, 'row 4'), (5, '5');

CREATE TABLE other (i INTEGER, t VARCHAR);

INSERT INTO other VALUES (2, 'two'), (4, 'four'), (7, 'seven');

CREATE TABLE other_schema.t (x INTEGER DEFAULT 3);

INSERT INTO other_schema.t VALUES (DEFAULT);

CREATE TABLE with_everything (
  i INTEGER NOT NULL,
  n INTEGER DEFAULT nextval('counter'),
  s VARCHAR DEFAULT 'it''s',
  m mood DEFAULT 'b',
  doubled INTEGER GENERATED ALWAYS AS (i * 2),
  label VARCHAR GENERATED ALWAYS AS (s || '/' || m::VARCHAR) VIRTUAL,
  CHECK (i > 0)
);

INSERT INTO with_everything (i) VALUES (1), (2);

CREATE MACRO twice(x) AS x * 2;

CREATE MACRO add_default(a, b := 1) AS a + b;

CREATE MACRO evens() AS TABLE SELECT i FROM base WHERE i % 2 = 0;

CREATE TABLE parent (id INTEGER PRIMARY KEY, code VARCHAR UNIQUE, a INTEGER, b INTEGER, UNIQUE (a, b));

INSERT INTO parent VALUES (1, 'one', 1, 1), (2, 'two', 1, 2);

CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id), manager INTEGER REFERENCES child (id));

INSERT INTO child VALUES (1, 1, NULL);

INSERT INTO child VALUES (2, 2, 1);

CREATE TABLE compound (x INTEGER, y VARCHAR, PRIMARY KEY (x, y));

INSERT INTO compound VALUES (1, 'a'), (1, 'b');

CREATE TABLE indexed (i INTEGER, s VARCHAR);

INSERT INTO indexed VALUES (1, 'a'), (2, 'B'), (2, 'c');

CREATE INDEX plain_index ON indexed (i);

CREATE UNIQUE INDEX unique_index ON indexed (s);

CREATE INDEX expression_index ON indexed (lower(s), (i + 1));

CREATE TABLE other_schema.keyed (k INTEGER PRIMARY KEY);

INSERT INTO other_schema.keyed VALUES (1);

CREATE INDEX other_index ON other_schema.keyed (k);
