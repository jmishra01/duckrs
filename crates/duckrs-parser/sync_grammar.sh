#!/usr/bin/env bash
# Copies DuckDB's PEG grammar into this crate. Run after moving the rust-port-baseline marker.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
duckdb="${1:-$here/../../../duckdb}"
cp "$duckdb/src/include/duckdb/parser/peg/inlined_grammar.gram" "$here/grammar/inlined_grammar.gram"
cp "$duckdb"/src/parser/peg/grammar/keywords/*.list "$here/grammar/keywords/"
echo "Synced grammar from $(git -C "$duckdb" rev-parse --short=11 HEAD)"
