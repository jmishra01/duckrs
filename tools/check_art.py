#!/usr/bin/env python3
"""Can the ART of duckrs read what the real DuckDB wrote.

Needs the reference engine: cargo build --release -p sqllogictest --features reference

    python3 tools/check_art.py
"""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(__file__).resolve().parent.parent
scratch = Path(tempfile.mkdtemp())
(scratch / "test").mkdir()
files = []
script = []
for name, options in [("small", "(BLOCK_SIZE 16384)"), ("large", "")]:
    target = scratch / f"{name}.db"
    files.append(str(target))
    script.append(f"""statement ok
ATTACH '{target}' AS {name} {options}

statement ok
CREATE TABLE {name}.t (i INTEGER PRIMARY KEY, s VARCHAR UNIQUE, g INTEGER, p VARCHAR, q BIGINT)

statement ok
INSERT INTO {name}.t SELECT i, 'https://example.com/items/' || i, i % 7, 'group-' || (i % 300), i % 11 FROM range(20000) r(i)

statement ok
CREATE INDEX idx_groups ON {name}.t (g)

statement ok
CREATE INDEX idx_pairs ON {name}.t (p, q)

statement ok
CHECKPOINT {name}

statement ok
DETACH {name}

""")
(scratch / "test/art.test").write_text("".join(script))
result = subprocess.run(
    [root / "target/release/sqllogictest", "--engine", "reference", "--duckdb-root", scratch, "test/art.test", "-v", "--continue-on-failure"],
    capture_output=True,
    text=True,
)
print(result.stdout[-1500:])
if result.returncode != 0:
    print(result.stderr[-1000:])
    sys.exit(result.returncode)
sys.exit(
    subprocess.run(
        ["cargo", "test", "-q", "-p", "duckrs-core", "an_art_that_duckdb_wrote", "--", "--ignored", "--nocapture"],
        cwd=root,
        env={**os.environ, "DUCKRS_ART_FILES": ":".join(files)},
    ).returncode
)
