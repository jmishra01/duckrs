# Upstream sync log

The port tracks one pinned DuckDB commit, marked by the `rust-port-baseline` branch in the
`duckdb/` checkout (fork `jmishra01/duckdb`). Moving the marker is a deliberate step: port the
diff for the affected modules, then record the move here.

```bash
cd ../duckdb
git fetch origin
git log --oneline rust-port-baseline..origin/main -- src/<module>
git diff rust-port-baseline..origin/main -- src/<module> test/sql/<area>
git branch -f rust-port-baseline <new-commit>
```

Also update `DUCKDB_BASELINE` in `.github/workflows/ci.yml`, so CI runs the matching test corpus.

| Date | From | To | Modules re-synced | Notes |
|---|---|---|---|---|
| 2026-11-03 | (none) | `4d77d9c103b` | n/a | Initial baseline. Storage version 69, vector size 2048. |
| 2026-11-28 | `4d77d9c103b` | `d4eea3f208d` | (recorded late) | The port and `known_passing.txt` had followed `d4eea3f208d` (the `duckdb/` checkout) for a while; CI and the benchmarks still checked out `4d77d9c103b`, which gave 30 false regressions. `rust-port-baseline` in `duckdb/` still points at `4d77d9c103b`. |
