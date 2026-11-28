#!/bin/sh
# Writes the database files of ../read_database_file.rs with the real DuckDB, and what it reads from
# them, so that the test compares pintail with DuckDB and not with itself.
#
#   DUCKDB=/path/to/duckdb crates/pintail/tests/fixtures/generate.sh
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

rm -f compatible.db compressed.db bitpacked.db floats.db modern.db nested.db deletes.db parsed_*.db wal_v*.db* scan_v*.db *.expected
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
expected floats.db alp alprd
expected modern.db validity dict_fsst
expected nested.db lists empty_lists structs arrays maps
expected deletes.db deletes

# Objects that are stored as parsed SQL: one view per line of stored_queries.txt, in two storage
# versions, which write constants and casts differently. (Hex literals are left out: 1.5.6 reads
# X'AB' as a string, and v2.0.0 as a blob.)
for version in v1.0.0 v1.5.0; do
  file=parsed_$(echo $version | tr -d .).db
  rm -f "$file"
  {
    cat <<SQL
ATTACH '$file' (BLOCK_SIZE 16384, STORAGE_VERSION '$version');
USE ${file%.db};
SQL
    cat stored_schema.sql
    n=0
    while IFS= read -r query; do
      n=$((n + 1))
      echo "CREATE VIEW q$n AS $query;"
    done < stored_queries.txt
    echo "CREATE VIEW aliased (first_name, second_name) AS SELECT 1 AS a, 2 AS b;"
    echo "CHECKPOINT;"
  } | "$DUCKDB"
  # the files of both versions hold the same rows, so that one set of expected rows does for both
  n=0
  while IFS= read -r query; do
    n=$((n + 1))
    columns=$("$DUCKDB" -readonly -noheader -list "$file" \
        -c "SELECT string_agg('\"' || column_name || '\"::VARCHAR', ', ' ORDER BY column_index)
            FROM duckdb_columns() WHERE table_name = 'q$n'")
    "$DUCKDB" -readonly -noheader -list -separator '|' -nullvalue NULL "$file" \
        -c "SET lambda_syntax = 'ENABLE_SINGLE_ARROW'; SELECT $columns FROM q$n" > "q$n.expected"
  done < stored_queries.txt
  "$DUCKDB" -readonly -noheader -list -separator '|' -nullvalue NULL "$file" \
      -c "SELECT sql FROM duckdb_views() WHERE NOT internal ORDER BY view_name" > views_sql.expected
  "$DUCKDB" -readonly -noheader -list -separator '|' -nullvalue NULL "$file" \
      -c "SELECT column_name, column_type, \"null\", \"default\" FROM (DESCRIBE with_everything)" > with_everything_columns.expected
done

# The CLI is too old for v2.0.0, where constants are literals and casts name parsed types. The
# reference engine of the test runner can write it (`cargo build --release -p sqllogictest --features
# reference`). Arrow lambdas stop binding from v2.0.0 on, so its file has the keyword.
runner=../../../../target/release/sqllogictest
scratch=$(mktemp -d)
mkdir "$scratch/test"
rm -f parsed_v200.db
{
  echo "statement ok"
  echo "ATTACH '$PWD/parsed_v200.db' (BLOCK_SIZE 16384, STORAGE_VERSION 'v2.0.0')"
  echo
  echo "statement ok"
  echo "USE parsed_v200"
  echo
  awk 'BEGIN { RS = ""; ORS = "\n\n" } { print "statement ok\n" $0 }' stored_schema.sql
  awk '{ gsub(/x -> x \+ 1/, "lambda x: x + 1"); print "statement ok\nCREATE VIEW q" NR " AS " $0 "\n" }' stored_queries.txt
  printf 'statement ok\nCREATE VIEW aliased (first_name, second_name) AS SELECT 1 AS a, 2 AS b\n\n'
  printf 'statement ok\nCHECKPOINT\n\nstatement ok\nUSE memory\n\nstatement ok\nDETACH parsed_v200\n'
} > "$scratch/test/v200.test"
"$runner" --engine reference --duckdb-root "$scratch" test/v200.test
rm -rf "$scratch"

# Changes that are only in a write-ahead log. Opening a file for writing replays its log and
# removes it, and 1.5.6 can't replay a logged index read-only, so DuckDB reads a copy. What it reads
# is what pintail has to read from all three files.
wal_tables="kept logged_schema.fresh constants repeated bulk logged_view"
for version in v1.0.0 v1.5.0; do
  file=wal_$(echo $version | tr -d .).db
  rm -f "$file" "$file.wal"
  {
    echo "ATTACH '$file' (BLOCK_SIZE 16384, STORAGE_VERSION '$version');"
    echo "USE ${file%.db};"
    cat wal_checkpointed.sql wal_logged.sql
  } | "$DUCKDB" > /dev/null
  scratch=$(mktemp -d)
  cp "$file" "$file.wal" "$scratch"
  for table in $wal_tables; do
    columns=$("$DUCKDB" -noheader -list "$scratch/$file" \
        -c "SELECT string_agg('\"' || column_name || '\"::VARCHAR', ', ' ORDER BY column_index)
            FROM duckdb_columns() WHERE schema_name || '.' || table_name = '$table' OR table_name = '$table'")
    "$DUCKDB" -noheader -list -separator '|' -nullvalue NULL "$scratch/$file" \
        -c "SELECT $columns FROM $table" > "wal_$(echo "$table" | tr . _).expected"
  done
  rm -rf "$scratch"
done

# v2.0.0 through the reference engine, as for `parsed_v200.db`.
scratch=$(mktemp -d)
mkdir "$scratch/test"
rm -f wal_v200.db wal_v200.db.wal
{
  echo "statement ok"
  echo "ATTACH '$PWD/wal_v200.db' (BLOCK_SIZE 16384, STORAGE_VERSION 'v2.0.0')"
  echo
  echo "statement ok"
  echo "USE wal_v200"
  echo
  cat wal_checkpointed.sql wal_logged.sql | awk 'BEGIN { RS = ""; ORS = "\n\n" } { print "statement ok\n" $0 }'
} > "$scratch/test/wal_v200.test"
"$runner" --engine reference --duckdb-root "$scratch" test/wal_v200.test
rm -rf "$scratch"

# Tables that scans read in place: two row groups, with what statistics can get wrong. The second
# file has fewer rows in small row groups, which don't line up with the chunks of a table in memory.
for row_group_size in 122880 4096; do
  file=scan_v$row_group_size.db
  rm -f "$file"
  {
    echo "ATTACH '$file' (BLOCK_SIZE 16384, ROW_GROUP_SIZE $row_group_size);"
    echo "USE ${file%.db};"
    if [ "$row_group_size" = 4096 ]; then
      sed 's/range(130000)/range(30000)/' scan_tables.sql
    else
      cat scan_tables.sql
    fi
  } | "$DUCKDB" > /dev/null
  n=0
  while IFS= read -r query; do
    n=$((n + 1))
    "$DUCKDB" -readonly -noheader -list -separator '|' -nullvalue NULL "$file" -c "$query" \
        > "scan_${row_group_size}_$n.expected"
  done < scan_queries.txt
done
