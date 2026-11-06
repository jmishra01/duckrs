#!/usr/bin/env python3
"""Has the real DuckDB read a file that duckrs wrote, and answer as duckrs does.

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
    ["cargo", "test", "-q", "-p", "duckrs", "--test", "write_database_file", "a_database_comes_back"],
    cwd=root,
    check=True,
    env={**os.environ, "DUCKRS_KEEP_WRITTEN": str(target)},
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
    ["cargo", "test", "-q", "-p", "duckrs", "--test", "write_database_file", "a_log_brings_back"],
    cwd=root,
    check=True,
    env={**os.environ, "DUCKRS_KEEP_LOGGED": str(logged)},
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

# floats in each of their codecs, forced, as otherwise the smallest wins
floats = scratch / "floats"
floats.mkdir()
subprocess.run(
    ["cargo", "test", "-q", "-p", "duckrs", "--test", "write_database_file", "floats_keep_their_bits"],
    cwd=root,
    check=True,
    env={**os.environ, "DUCKRS_KEEP_FLOATS": str(floats)},
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
(scratch / "test/floats.test").write_text(test)
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/floats.test", "-v", "--continue-on-failure"],
    capture_output=True,
    text=True,
)
print(result.stdout[-4000:])
sys.exit(result.returncode)
