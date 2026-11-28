#!/bin/sh
# Builds a dsdgen that writes the pipe-delimited .tbl files examples/tpcds.rs loads, from the
# generators in a DuckDB checkout: the TPC code stays there, and driver.cpp replaces the part that
# appends to DuckDB tables. pintail has no dsdgen of its own yet.
#
#   tools/tpcds/build_dsdgen.sh <duckdb checkout> <output binary>
#   <output binary> <scale factor> <empty directory>
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dsdgen="$1/extension/tpcds/dsdgen"
out="$2"
sources=$(ls "$dsdgen"/dsdgen-c/*.cpp)
# shellcheck disable=SC2086
c++ -std=c++17 -O2 -w -I"$here/stub" -I"$dsdgen/include" -I"$dsdgen/include/dsdgen-c" \
	$sources "$here/driver.cpp" -o "$out"
