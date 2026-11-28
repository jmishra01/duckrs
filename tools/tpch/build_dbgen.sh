#!/bin/sh
# Builds a dbgen that writes the pipe-delimited .tbl files examples/tpch.rs loads, from the
# generators in a DuckDB checkout: the TPC code stays there, and the row functions of its
# dbgen.cpp get text output instead of DuckDB's appender. pintail has no dbgen of its own yet.
#
#   tools/tpch/build_dbgen.sh <duckdb checkout> <output binary>
#   <output binary> <scale factor> <empty directory>
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dbgen="$1/extension/tpch/dbgen"
out="$2"
source=$(mktemp -d)/driver.cpp
first=$(grep -n '^static int PaddedNumberWidth' "$dbgen/dbgen.cpp" | cut -d: -f1)
last=$(grep -n '^static void gen_tbl' "$dbgen/dbgen.cpp" | cut -d: -f1)
{
	cat "$here/driver_head.cpp"
	sed -n "${first},$((last - 1))p" "$dbgen/dbgen.cpp"
	cat "$here/driver_tail.cpp"
} >"$source"
c++ -std=c++17 -O2 -w -I"$here/stub" -I"$dbgen/../include" -I"$dbgen/include" \
	"$dbgen/bm_utils.cpp" "$dbgen/build.cpp" "$dbgen/dbgen_gunk.cpp" "$dbgen/permute.cpp" "$dbgen/rnd.cpp" \
	"$dbgen/rng64.cpp" "$dbgen/speed_seed.cpp" "$dbgen/text.cpp" "$source" -o "$out"
