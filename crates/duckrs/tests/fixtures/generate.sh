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

rm -f compatible.db compressed.db bitpacked.db floats.db modern.db nested.db deletes.db *.expected
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

# Floats: DuckDB uses ALP and ALPRD only in blocks of the default size. Then what is new since v1.0.
"$DUCKDB" <<SQL
ATTACH 'floats.db';
USE floats;
PRAGMA force_compression = 'alp';
CREATE TABLE alp (d DOUBLE, f FLOAT, exceptions DOUBLE);
INSERT INTO alp SELECT i / 100, (i % 1000) / 8, CASE WHEN i % 50 = 0 THEN 1 / 3 * i WHEN i % 7 = 0 THEN NULL ELSE i / 1000 END
  FROM range(5000) r(i);
INSERT INTO alp VALUES ('NaN', 'NaN', 'infinity'), (0.0, -0.0, '-infinity'), (1e300, 1e30, 1e-300);
CHECKPOINT;
PRAGMA force_compression = 'alprd';
CREATE TABLE alprd (d DOUBLE, f FLOAT);
INSERT INTO alprd SELECT sqrt(i) * 3.14159, sqrt(i)::FLOAT FROM range(5000) r(i);
CHECKPOINT;
PRAGMA force_compression = 'auto';
SQL

"$DUCKDB" <<SQL
ATTACH 'modern.db' (BLOCK_SIZE 16384, STORAGE_VERSION 'v1.5.0');
USE modern;
PRAGMA force_compression = 'roaring';
CREATE TABLE validity (few_nulls INTEGER, few_values INTEGER, runs INTEGER, many_runs INTEGER, bits INTEGER, b BOOLEAN);
INSERT INTO validity SELECT
  CASE WHEN i % 1000 = 0 OR i % 1009 = 0 THEN NULL ELSE i END,
  CASE WHEN i % 1000 = 0 OR i % 1009 = 0 THEN i END,
  CASE WHEN i % 4000 < 1000 THEN NULL ELSE i END,
  CASE WHEN i % 300 < 20 THEN NULL ELSE i END,
  CASE WHEN (i * 2654435761) % 4294967296 % 5 < 2 THEN NULL ELSE i END,
  CASE WHEN i % 7 = 0 THEN NULL ELSE (i * 2654435761) % 4294967296 % 3 = 0 END
  FROM range(9000) r(i);
CHECKPOINT;
PRAGMA force_compression = 'dict_fsst';
CREATE TABLE dict_fsst (few VARCHAR, unique_strings VARCHAR, n VARCHAR);
INSERT INTO dict_fsst SELECT 'category number ' || (i % 20), 'unique value that compresses well ' || i,
  CASE WHEN i % 3 = 0 THEN NULL WHEN i % 3 = 1 THEN '' ELSE 'x' END FROM range(4000) r(i);
CHECKPOINT;
PRAGMA force_compression = 'auto';
SQL

"$DUCKDB" <<SQL
ATTACH 'nested.db' (BLOCK_SIZE 16384);
USE nested;
CREATE TABLE lists (l INTEGER[], s VARCHAR[], ll INTEGER[][]);
INSERT INTO lists SELECT
  CASE WHEN i % 5 = 0 THEN NULL WHEN i % 5 = 1 THEN [] ELSE [i, NULL, i + 1] END,
  CASE WHEN i % 7 = 0 THEN NULL ELSE ['a' || i, NULL, ''] END,
  [[i], [], NULL, [i, i]] FROM range(3000) r(i);
CREATE TABLE empty_lists (l INTEGER[]);
INSERT INTO empty_lists SELECT [] FROM range(10);
CREATE TABLE structs (s STRUCT(a INTEGER, b VARCHAR), n STRUCT(x STRUCT(y INTEGER[], z DOUBLE)));
INSERT INTO structs SELECT CASE WHEN i % 5 = 0 THEN NULL ELSE {'a': i, 'b': CASE WHEN i % 2 = 0 THEN NULL ELSE 'v' || i END} END,
  {'x': {'y': [i], 'z': i / 2}} FROM range(3000) r(i);
CREATE TABLE arrays (a INTEGER[3], s VARCHAR[2], aa INTEGER[2][2]);
INSERT INTO arrays SELECT [i, NULL, i + 2]::INTEGER[3], ['x' || i, NULL]::VARCHAR[2],
  [[i, i], [i, NULL]]::INTEGER[2][2] FROM range(3000) r(i);
INSERT INTO arrays SELECT NULL, ['y' || i, 'z']::VARCHAR[2], NULL FROM range(100) r(i);
CREATE TABLE maps (m MAP(VARCHAR, INTEGER), u UNION(num INTEGER, str VARCHAR), e ENUM('sad', 'ok', 'happy'));
INSERT INTO maps SELECT CASE WHEN i % 5 = 0 THEN NULL ELSE map(['k' || i, 'other'], [i, NULL]) END,
  CASE WHEN i % 2 = 0 THEN union_value(num := i)::UNION(num INTEGER, str VARCHAR)
       ELSE union_value(str := 's' || i)::UNION(num INTEGER, str VARCHAR) END,
  (['sad', 'ok', 'happy'])[i % 3 + 1]::ENUM('sad', 'ok', 'happy') FROM range(3000) r(i);
CHECKPOINT;
SQL

"$DUCKDB" <<SQL
ATTACH 'deletes.db' (BLOCK_SIZE 16384);
USE deletes;
CREATE TABLE deletes (i INTEGER, s VARCHAR);
INSERT INTO deletes SELECT i, 's' || (i % 10) FROM range(30000) r(i);
CHECKPOINT;
DELETE FROM deletes WHERE i % 97 = 0 OR i BETWEEN 2048 AND 4095 OR i BETWEEN 4100 AND 6000;
CHECKPOINT;
SQL

# FLOATs go through DOUBLE, to compare their values: duckrs and DuckDB can print one differently.
expected() { # file table...
    file=$1
    shift
    for table in "$@"; do
        name=$(echo "$table" | tr . _)
        columns=$("$DUCKDB" -readonly -noheader -list "$file" \
            -c "SELECT string_agg('\"' || column_name || '\"' || if(data_type = 'FLOAT', '::DOUBLE', '') || '::VARCHAR', ', ' ORDER BY ordinal_position)
                FROM information_schema.columns
                WHERE table_schema || '.' || table_name = '$table' OR table_name = '$table'")
        "$DUCKDB" -readonly -noheader -list -separator '|' -nullvalue NULL "$file" \
            -c "SELECT $columns FROM $table" > "$name.expected"
    done
}
expected compatible.db other.t uncompressed long_strings empty_table
expected compressed.db rle dictionary fsst fsst_empty constant
expected bitpacked.db bitpacking groups
expected floats.db alp alprd
expected modern.db validity dict_fsst
expected nested.db lists empty_lists structs arrays maps
expected deletes.db deletes
