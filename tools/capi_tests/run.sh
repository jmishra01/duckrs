#!/usr/bin/env bash
# Runs DuckDB's C API tests (test/api/capi/v1/test_capi*.cpp and the capi_*_functions.cpp of user-defined functions,
# of the C++ checkout) against libduckrs_capi.
#
# usage: tools/capi_tests/run.sh [--release] [test_capi.cpp ...]
#
# Each test file is compiled with Catch2 and duckdb.h from the checkout and the shim of shim/ (in place of DuckDB's
# C++ test helpers), and linked with libduckrs_capi plus generated weak stubs for the duckdb.h functions the library
# does not export yet (a stub throws, so only the test cases that call it fail). Every test case then runs in a
# process of its own, so that one that crashes does not take the others with it.
#
# DUCKDB_SOURCE: the C++ checkout (default: the first `duckdb` directory next to this worktree or above it).
# Build output goes to target/capi_tests (CAPI_TEST_BUILD to change it); the checkout is only read.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

PROFILE=debug
CARGO_FLAGS=()
if [[ "${1:-}" == "--release" ]]; then
	PROFILE=release
	CARGO_FLAGS=(--release)
	shift
fi

if [[ -z "${DUCKDB_SOURCE:-}" ]]; then
	dir=$ROOT
	while [[ "$dir" != "/" ]]; do
		if [[ -f "$dir/duckdb/src/include/duckdb.h" ]]; then
			DUCKDB_SOURCE="$dir/duckdb"
			break
		fi
		dir=$(dirname "$dir")
	done
fi
if [[ -z "${DUCKDB_SOURCE:-}" || ! -f "$DUCKDB_SOURCE/src/include/duckdb.h" ]]; then
	echo "set DUCKDB_SOURCE to the DuckDB C++ checkout" >&2
	exit 1
fi

TESTS=("$@")
if [[ ${#TESTS[@]} -eq 0 ]]; then
	TESTS=(test_capi.cpp test_capi_data_chunk.cpp test_capi_prepared.cpp test_capi_appender.cpp test_capi_values.cpp
		test_capi_complex_types.cpp test_capi_vector.cpp test_capi_website.cpp test_capi_extract.cpp
		test_capi_append_data_chunk.cpp test_capi_to_decimal.cpp capi_scalar_functions.cpp capi_aggregate_functions.cpp
		capi_table_functions.cpp test_capi_replacement_scan.cpp test_capi_table_description.cpp test_capi_arrow.cpp
		capi_custom_type.cpp capi_copy_function.cpp capi_file_system.cpp test_capi_profiling.cpp test_capi_logging.cpp
		test_capi_streaming.cpp test_capi_instance_cache.cpp capi_catalog.cpp capi_config_options.cpp test_capi_geometry.cpp)
fi

BUILD=${CAPI_TEST_BUILD:-$ROOT/target/capi_tests}
mkdir -p "$BUILD/src" "$BUILD/obj" "$BUILD/bin"

echo "== building libduckrs_capi ($PROFILE)"
(cd "$ROOT" && cargo build -p duckrs-capi "${CARGO_FLAGS[@]}")
LIBDIR="$ROOT/target/$PROFILE"
case "$(uname)" in
Darwin)
	LIB="$LIBDIR/libduckrs_capi.dylib"
	nm -gU "$LIB" | awk '{print $3}' | sed 's/^_//' >"$BUILD/exported.txt"
	;;
*)
	LIB="$LIBDIR/libduckrs_capi.so"
	nm -D --defined-only "$LIB" | awk '{print $3}' >"$BUILD/exported.txt"
	;;
esac

python3 "$HERE/gen_stubs.py" stubs "$DUCKDB_SOURCE/src/include/duckdb.h" "$BUILD/exported.txt" "$BUILD/src/stubs.cpp"
python3 "$HERE/gen_stubs.py" list "$DUCKDB_SOURCE/src/include/duckdb.h" >"$BUILD/all_functions.txt"
grep -Fxf "$BUILD/all_functions.txt" "$BUILD/exported.txt" | sort >"$BUILD/implemented.txt" || true
echo "== $(wc -l <"$BUILD/implemented.txt" | tr -d ' ') of $(wc -l <"$BUILD/all_functions.txt" | tr -d ' ') duckdb.h functions implemented"

CXX=${CXX:-clang++}
CXXFLAGS=(-std=c++17 -O0 -g -w -I "$HERE/shim" -I "$DUCKDB_SOURCE/src/include" -I "$DUCKDB_SOURCE/third_party/catch")

compile() { # source object
	if [[ ! -f "$2" || "$1" -nt "$2" || "$HERE/shim/capi_tester.hpp" -nt "$2" || "$HERE/shim/test_helpers.hpp" -nt "$2" ]]; then
		"$CXX" "${CXXFLAGS[@]}" -c "$1" -o "$2"
	fi
}

compile "$HERE/shim/catch_main.cpp" "$BUILD/obj/catch_main.o"
compile "$HERE/shim/capi_tester.cpp" "$BUILD/obj/capi_tester.o"
"$CXX" "${CXXFLAGS[@]}" -c "$BUILD/src/stubs.cpp" -o "$BUILD/obj/stubs.o"

BINARIES=()
for test in "${TESTS[@]}"; do
	name=${test%.cpp}
	python3 "$HERE/prepare_test.py" "$HERE/excluded.txt" "$DUCKDB_SOURCE/test/api/capi/v1/$test" "$BUILD/src/$test"
	echo "== compiling $test"
	if ! "$CXX" "${CXXFLAGS[@]}" -c "$BUILD/src/$test" -o "$BUILD/obj/$name.o" 2>"$BUILD/obj/$name.log"; then
		echo "   does not compile, see $BUILD/obj/$name.log"
		continue
	fi
	"$CXX" -o "$BUILD/bin/$name" "$BUILD/obj/$name.o" "$BUILD/obj/catch_main.o" "$BUILD/obj/capi_tester.o" \
		"$BUILD/obj/stubs.o" -L "$LIBDIR" -lduckrs_capi -Wl,-rpath,"$LIBDIR"
	BINARIES+=("$BUILD/bin/$name")
done

mkdir -p "$BUILD/work"
cd "$BUILD/work"
python3 "$HERE/run_cases.py" "$BUILD/results.md" "${BINARIES[@]}"
