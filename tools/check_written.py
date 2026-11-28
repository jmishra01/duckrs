#!/usr/bin/env python3
"""Has the real DuckDB read a file that pintail wrote, and answer as pintail does.

Needs the reference engine: cargo build --release -p sqllogictest --features reference

    python3 tools/check_written.py
"""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(__file__).resolve().parent.parent
scratch = Path(tempfile.mkdtemp())
target = scratch / "written.db"
subprocess.run(
    ["cargo", "test", "-q", "-p", "pintail", "--test", "write_database_file", "a_database_comes_back"],
    cwd=root,
    check=True,
    env={**os.environ, "PINTAIL_KEEP_WRITTEN": str(target)},
)
test = [f"statement ok\nATTACH '{target}' AS written (READ_ONLY)\n\nstatement ok\nUSE written\n\n"]
for block in Path(f"{target}.expected").read_text().split("\n\n"):
    lines = block.splitlines()
    if not lines:
        continue
    sql, rows = lines[0], lines[1:]
    columns = rows[0].count("|") + 1 if rows else 1
    expected = "\n".join("\t".join(field or "(empty)" for field in r.split("|")) for r in rows)
    test.append(f"query {'T' * columns} nosort\n{sql}\n----\n{expected}\n\n")
(scratch / "test").mkdir()
(scratch / "test/written.test").write_text("".join(test))
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/written.test", "-v", "--continue-on-failure"],
    capture_output=True,
    text=True,
)
print(result.stdout[-4000:])
print(result.stderr[-1000:])
if result.returncode != 0:
    sys.exit(result.returncode)

# DuckDB frees and reuses blocks by what the file says of them, which reading doesn't exercise
(scratch / "test/writes.test").write_text(f"""statement ok
ATTACH '{target}' AS written

statement ok
UPDATE written.big SET s = 'changed' WHERE i % 10 = 0

statement ok
DELETE FROM written.big WHERE i % 10 = 1

statement ok
INSERT INTO written.big SELECT i + 250000, 'added', NULL FROM range(50000) r(i)

statement ok
DROP TABLE written.long_strings

statement ok
CHECKPOINT written

statement ok
UPDATE written.big SET s = 'again' WHERE i % 10 = 2

statement ok
CHECKPOINT written

statement ok
DETACH written

statement ok
ATTACH '{target}' AS written (READ_ONLY)

query II
SELECT count(*), count(*) FILTER (WHERE s = 'changed') FROM written.big
----
275000	25000

query I
SELECT count(*) FROM written.big WHERE s = 'again'
----
30000

query I
SELECT count(*) FROM written.v
----
9

statement error
SELECT * FROM written.long_strings
----
does not exist
""")
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/writes.test", "-v", "--continue-on-failure"],
    capture_output=True,
    text=True,
)
print(result.stdout[-4000:])
if result.returncode != 0:
    sys.exit(result.returncode)

# a file and a log: what the database holds is only there if DuckDB replays the log
logged = scratch / "logged.db"
subprocess.run(
    ["cargo", "test", "-q", "-p", "pintail", "--test", "write_database_file", "a_log_brings_back"],
    cwd=root,
    check=True,
    env={**os.environ, "PINTAIL_KEEP_LOGGED": str(logged)},
)
(scratch / "test/log.test").write_text(f"""statement ok
ATTACH '{logged}' AS logged

query I
SELECT count(*) FROM logged.big
----
214285

query I
SELECT count(*) FROM logged.big WHERE s = 'updated'
----
42857

query I
SELECT sum(i) FROM logged.big WHERE i < 10
----
38

query I
SELECT count(*) FROM logged.types
----
3

query II
SELECT x, z FROM logged.other.t ORDER BY x
----
1	6
2	7

query I
SELECT logged.main.twice(4)
----
8

query I
SELECT count(*) FROM logged.empty_table
----
0

statement error
SELECT * FROM logged.gone
----
does not exist

query I
SELECT nextval('logged.counter')
----
12
""")
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/log.test", "-v", "--continue-on-failure"],
    capture_output=True,
    text=True,
)
print(result.stdout[-4000:])
if result.returncode != 0:
    sys.exit(result.returncode)

# ids: DuckDB reads rows with gaps by the ids they have here, and replays a log that names them
gaps = scratch / "gaps.db"
subprocess.run(
    ["cargo", "test", "-q", "-p", "pintail", "--test", "row_ids", "gaps_for_duckdb"],
    cwd=root,
    check=True,
    env={**os.environ, "PINTAIL_KEEP_GAPS": str(gaps)},
)
replayed = Path(f"{gaps}.expected").read_text().splitlines()[1]
(scratch / "test/gaps.test").write_text(f"""statement ok
ATTACH '{gaps}' AS gaps

query TTTTTT
SELECT count(*), min(rowid), max(rowid), sum(rowid), sum(i), count(*) FILTER (s = 'changed') FROM gaps.gaps
----
{replayed}

query I
SELECT count(*) FROM gaps.gaps WHERE rowid BETWEEN 2048 AND 4095
----
0

query T
SELECT DISTINCT s = 'row ' || i OR s IN ('changed', 'added') OR i >= 1000000 FROM gaps.gaps
----
true

query I
SELECT count(*) FROM gaps.gaps a JOIN gaps.gaps b ON a.rowid = b.rowid WHERE a.i = 5002
----
1

statement ok
INSERT INTO gaps.gaps VALUES (-5, 'more', [])

query T
SELECT rowid > 12287 FROM gaps.gaps WHERE i = -5
----
true

statement error
INSERT INTO gaps.gaps VALUES (5002, 'again', [])
----
Duplicate key

statement ok
CHECKPOINT gaps

statement ok
DETACH gaps
""")
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/gaps.test"],
    capture_output=True,
    text=True,
)
print(result.stdout[-4000:])
if result.returncode != 0:
    sys.exit(result.returncode)

# keys and indexes: DuckDB finds every key in the ART that pintail wrote, and goes on changing them
keyed = scratch / "keyed.db"
logged_keys = scratch / "keyed_log.db"
subprocess.run(
    ["cargo", "test", "-q", "-p", "pintail", "--test", "write_database_file", "keys_and_indexes"],
    cwd=root,
    check=True,
    env={**os.environ, "PINTAIL_KEEP_KEYS": str(keyed), "PINTAIL_KEEP_KEYS_LOGGED": str(logged_keys)},
)
altered = scratch / "altered.db"
subprocess.run(
    ["cargo", "test", "-q", "-p", "pintail", "--test", "write_database_file", "a_referenced_table"],
    cwd=root,
    check=True,
    env={**os.environ, "PINTAIL_KEEP_KEYS_ALTERED": str(altered)},
)


def key_checks(name, parents, pairs):
    return f"""query II
SELECT count(*), count(DISTINCT id) FROM {name}.parent
----
{parents}\t{parents}

statement error
INSERT INTO {name}.parent VALUES (5, 'x', 'n', 0)
----
Duplicate key

statement error
INSERT INTO {name}.parent VALUES (500000, 'https://example.com/items/7', 'n', 0)
----
Duplicate key

statement error
INSERT INTO {name}.a_child VALUES (9999, 123456, NULL)
----
foreign key

statement error
INSERT INTO {name}.a_child VALUES (9999, 1, 123456)
----
foreign key

statement error
DELETE FROM {name}.parent WHERE id = 1
----
still referenced

statement ok
INSERT OR IGNORE INTO {name}.parent SELECT i, 'by id ' || i, 'x', 0 FROM range(20000) r(i)

statement ok
INSERT OR IGNORE INTO {name}.parent SELECT i + 100000, 'https://example.com/items/' || i, 'x', 0 FROM range(20000) r(i)

query I
SELECT count(*) FROM {name}.parent
----
{parents}

query T
SELECT name FROM {name}.parent WHERE id = 12345
----
https://example.com/items/12345

query I
SELECT id FROM {name}.parent WHERE name = 'https://example.com/items/777'
----
777

query I
SELECT count(*) FROM {name}.pairs
----
{pairs}

statement error
INSERT INTO {name}.pairs VALUES (3, 'pair 3', 0)
----
Duplicate key

"""


(scratch / "test/keys.test").write_text(f"""statement ok
ATTACH '{keyed}' AS keyed

{key_checks("keyed", 20001, 3000)}
query I
SELECT id + 1 FROM keyed.parent WHERE id + 1 = 100
----
100

statement error
INSERT INTO keyed.parent VALUES (-5, 'other', 'n', 0), (-5, 'another', 'n', 0)
----
duplicate key

query I
SELECT count(*) FROM keyed.parent WHERE note = 'note 3'
----
2857

query I
SELECT count(*) FROM duckdb_indexes() WHERE database_name = 'keyed'
----
3

statement ok
INSERT INTO keyed.parent VALUES (40000, 'added', 'note 3', 2)

statement ok
DELETE FROM keyed.pairs WHERE a = 1

statement ok
UPDATE keyed.pairs SET c = 0 WHERE a = 2

statement ok
INSERT INTO keyed.pairs SELECT 1, 'pair ' || i, 0 FROM range(60) r(i)

statement ok
CHECKPOINT keyed

statement error
INSERT INTO keyed.parent VALUES (40000, 'x', 'n', 0)
----
Duplicate key

statement ok
DETACH keyed

statement ok
ATTACH '{keyed}' AS keyed

statement error
INSERT INTO keyed.pairs VALUES (1, 'pair 7', 0)
----
Duplicate key

statement ok
DROP TABLE keyed.a_child

statement ok
DROP TABLE keyed.parent

statement ok
CHECKPOINT keyed

statement ok
DETACH keyed

statement ok
ATTACH '{keyed}' AS keyed (READ_ONLY)

query I
SELECT count(*) FROM keyed.pairs
----
3000

""")
(scratch / "test/keys_log.test").write_text(f"""statement ok
ATTACH '{logged_keys}' AS logged_keys

{key_checks("logged_keys", 20002, 2940)}
query T
SELECT note FROM logged_keys.parent WHERE id = 3
----
changed

statement error
INSERT INTO logged_keys.later VALUES (2)
----
Duplicate key

query I
SELECT index_name FROM duckdb_indexes() WHERE database_name = 'logged_keys' ORDER BY 1
----
by_a
by_note
by_sum
later_i

statement ok
CHECKPOINT logged_keys

statement ok
DETACH logged_keys

statement ok
ATTACH '{logged_keys}' AS logged_keys (READ_ONLY)

query I
SELECT count(*) FROM logged_keys.pairs
----
2940
""")
(scratch / "test/keys_altered.test").write_text(f"""statement ok
ATTACH '{altered}' AS altered

query II
SELECT count(*), count(j) FROM altered.p
----
3\t1

statement error
INSERT INTO altered.c VALUES (9)
----
foreign key

statement error
DELETE FROM altered.p WHERE i = 1
----
still referenced

statement error
INSERT INTO altered.p VALUES (3, 0)
----
Duplicate key

query I
SELECT index_name FROM duckdb_indexes() WHERE database_name = 'altered'
----
c_i

statement ok
INSERT INTO altered.c VALUES (3)

statement ok
CHECKPOINT altered
""")
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/keys.test", "test/keys_log.test", "test/keys_altered.test", "-v", "--continue-on-failure"],
    capture_output=True,
    text=True,
)
print(result.stdout[-6000:])
print(result.stderr[-2000:])
if result.returncode != 0:
    sys.exit(result.returncode)

# floats in each of their codecs, forced, as otherwise the smallest wins
floats = scratch / "floats"
floats.mkdir()
subprocess.run(
    ["cargo", "test", "-q", "-p", "pintail", "--test", "write_database_file", "--", "floats_keep_their_bits", "strings_can_go_through_fsst"],
    cwd=root,
    check=True,
    env={**os.environ, "PINTAIL_KEEP_FLOATS": str(floats)},
)
test = "".join(
    f"""statement ok
ATTACH '{floats}/{force}.db' AS {force} (READ_ONLY)

query I
SELECT DISTINCT compression FROM pragma_storage_info('{force}.t') WHERE column_path IN ('[1]', '[2]') AND segment_type <> 'VALIDITY'
----
{name}

query I
SELECT count(*) FROM uncompressed.t a JOIN {force}.t b USING (i) WHERE a.d::VARCHAR = b.d::VARCHAR AND a.f::VARCHAR = b.f::VARCHAR
----
6000

"""
    for force, name in [("uncompressed", "Uncompressed"), ("alp", "ALP"), ("alprd", "ALPRD")]
)
# which has to be there before the first comparison
test = test.replace(f"ATTACH '{floats}/uncompressed.db' AS uncompressed (READ_ONLY)", "SELECT 1", 1)
test = f"statement ok\nATTACH '{floats}/uncompressed.db' AS uncompressed (READ_ONLY)\n\n" + test
# strings through FSST, against the same strings as DuckDB makes them: nothing here comes from pintail
test += f"""statement ok
ATTACH '{floats}/strings.db' AS strings (READ_ONLY)

query I
SELECT count(*) > 0 FROM pragma_storage_info('strings.u', include_segment_info = true) WHERE segment_type = 'VARCHAR' AND segment_info LIKE '%FSST_ONLY%'
----
true

query I
SELECT count(*) > 0 FROM pragma_storage_info('strings.r', include_segment_info = true) WHERE segment_type = 'VARCHAR' AND segment_info LIKE '%DICT_FSST%'
----
true

query I
SELECT count(*) FROM strings.u a JOIN (SELECT i, 'https://example.com/items/' || i AS s FROM range(50000) r(i)) b USING (i) WHERE a.s = b.s
----
50000

query I
SELECT count(*) FROM strings.r a JOIN (SELECT i, CASE WHEN i % 7 = 0 THEN NULL WHEN i % 11 = 0 THEN '' ELSE 'customer-segment-' || (i % 3000) || '-ünï' END AS s FROM range(50000) r(i)) b USING (i) WHERE a.s IS NOT DISTINCT FROM b.s
----
50000

"""
(scratch / "test/floats.test").write_text(test)
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/floats.test", "-v", "--continue-on-failure"],
    capture_output=True,
    text=True,
)
print(result.stdout[-4000:])
sys.exit(result.returncode)
