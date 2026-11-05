#!/bin/sh
# Writes the database files of ../read_database_file.rs with the real DuckDB, and what it reads from
# them, so that the test compares duckrs with DuckDB and not with itself.
#
#   DUCKDB=/path/to/duckdb crates/duckrs/tests/fixtures/generate.sh
set -eu
cd "$(dirname "$0")"
DUCKDB=${DUCKDB:-duckdb}

COLUMNS="b BOOLEAN, ti TINYINT, si SMALLINT, i INTEGER, bi BIGINT, ut UTINYINT, us USMALLINT, ui UINTEGER,
  ub UBIGINT, h HUGEINT, d DATE, ts TIMESTAMP, t TIME, d4 DECIMAL(9,2), d8 DECIMAL(18,4), d16 DECIMAL(38,6),
  u UUID, iv INTERVAL, s VARCHAR, bl BLOB"
ROWS="SELECT
  CASE WHEN i % 7 = 0 THEN NULL ELSE i % 3 = 0 END,
  (i % 100 - 50)::TINYINT, (i % 1000 - 500)::SMALLINT, i * 3 - 1000, i * 10000000000 - 5, (i % 200)::UTINYINT,
  (i % 40000)::USMALLINT, i * 7, i * 10000000001, i::HUGEINT * 100000000000000000000 - 7,
  DATE '2000-01-01' + (i % 5000)::INT, TIMESTAMP '2000-01-01 00:00:00' + INTERVAL (i) SECOND,
  TIME '00:00:00' + INTERVAL (i % 86400) SECOND, (i % 100000 / 100)::DECIMAL(9,2), (i * 3 / 7)::DECIMAL(18,4),
  (i * 11 / 3)::DECIMAL(38,6), ('00000000-0000-4000-8000-' || lpad(i::VARCHAR, 12, '0'))::UUID,
  INTERVAL (i % 12) MONTH + INTERVAL (i % 28) DAY + INTERVAL (i % 1000) SECOND,
  CASE WHEN i % 11 = 0 THEN NULL WHEN i % 13 = 0 THEN '' ELSE 'row number ' || (i % 50) END,
  ('blob' || (i % 9))::BLOB
  FROM range(COUNT_ROWS) r(i)"

rm -f compatible.db compressed.db bitpacked.db *.expected
"$DUCKDB" <<SQL
ATTACH 'compatible.db' (BLOCK_SIZE 16384);
USE compatible;
CREATE SCHEMA other;
CREATE TABLE empty_table (i INTEGER, s VARCHAR);
CREATE TABLE other.t (i INTEGER NOT NULL, s VARCHAR);
INSERT INTO other.t VALUES (1, 'in another schema'), (2, NULL);
PRAGMA force_compression = 'uncompressed';
CREATE TABLE uncompressed ($COLUMNS);
INSERT INTO uncompressed $(echo "$ROWS" | sed s/COUNT_ROWS/5000/);
CREATE TABLE long_strings (i INTEGER, s VARCHAR);
INSERT INTO long_strings SELECT i, repeat(chr(97 + i::INT), 9000 * i::INT) FROM range(6) r(i);
CHECKPOINT;
PRAGMA force_compression = 'auto';
SQL

"$DUCKDB" <<SQL
ATTACH 'compressed.db' (BLOCK_SIZE 16384);
USE compressed;
PRAGMA force_compression = 'rle';
CREATE TABLE rle (i INTEGER, b BIGINT, n SMALLINT);
INSERT INTO rle SELECT i // 100, i // 1000, CASE WHEN i % 10 < 3 THEN NULL ELSE 3 END FROM range(10000) r(i);
CHECKPOINT;
PRAGMA force_compression = 'dictionary';
CREATE TABLE dictionary (i INTEGER, s VARCHAR);
INSERT INTO dictionary SELECT i, CASE WHEN i % 9 = 0 THEN NULL WHEN i % 7 = 0 THEN '' ELSE 'value ' || (i % 40) END
  FROM range(8000) r(i);
CHECKPOINT;
PRAGMA force_compression = 'fsst';
CREATE TABLE fsst (i INTEGER, s VARCHAR);
INSERT INTO fsst SELECT i, CASE WHEN i % 9 = 0 THEN NULL WHEN i % 7 = 0 THEN ''
  ELSE 'https://example.com/some/long/path/' || (i * 37 % 1000) || '/index.html?héllo=wörld' END FROM range(8000) r(i);
CREATE TABLE fsst_empty (s VARCHAR);
INSERT INTO fsst_empty SELECT '' FROM range(100);
CHECKPOINT;
PRAGMA force_compression = 'auto';
CREATE TABLE constant (i INTEGER, s VARCHAR, n INTEGER);
INSERT INTO constant SELECT 42, NULL, NULL FROM range(3000);
CHECKPOINT;
SQL

# DuckDB bitpacks 32-bit and wider values only in blocks of the default size.
"$DUCKDB" <<SQL
ATTACH 'bitpacked.db';
USE bitpacked;
PRAGMA force_compression = 'bitpacking';
CREATE TABLE bitpacking (a INTEGER, delta BIGINT, c TINYINT, h HUGEINT, n USMALLINT, b BOOLEAN, neg BIGINT);
INSERT INTO bitpacking SELECT i * 3 + 7, i * i, 5, i::HUGEINT * 1000000000000000000000,
  CASE WHEN i % 5 = 0 THEN NULL ELSE (i % 60000) END, i % 3 = 0, (i - 6000) * 1000 FROM range(12000) r(i);
CREATE TABLE groups (constant_group BIGINT, wide BIGINT, u UINTEGER);
INSERT INTO groups SELECT CASE WHEN i < 2048 THEN 7 ELSE i END, CASE WHEN i % 2 = 0 THEN i ELSE -i * 1000000 END,
  (i * 2654435761) % 4294967296 FROM range(7000) r(i);
CHECKPOINT;
SQL

expected() { # file table...
    file=$1
    shift
    for table in "$@"; do
        name=$(echo "$table" | tr . _)
        columns=$("$DUCKDB" -readonly -noheader -list "$file" \
            -c "SELECT string_agg('\"' || column_name || '\"::VARCHAR', ', ' ORDER BY ordinal_position)
                FROM information_schema.columns
                WHERE table_schema || '.' || table_name = '$table' OR table_name = '$table'")
        "$DUCKDB" -readonly -noheader -list -separator '|' -nullvalue NULL "$file" \
            -c "SELECT $columns FROM $table" > "$name.expected"
    done
}
expected compatible.db other.t uncompressed long_strings empty_table
expected compressed.db rle dictionary fsst fsst_empty constant
expected bitpacked.db bitpacking groups
