//===----------------------------------------------------------------------===//
// duckrs: DuckDB's test/helpers/capi_tester.cpp (NO_FAIL and CAPIResult::Fetch, unchanged), and the helpers of
// test_helpers.hpp the C API tests use, written against the C API and the standard library.
//===----------------------------------------------------------------------===//

#include "capi_tester.hpp"

#include <cerrno>
#include <cstdlib>
#include <sys/stat.h>
#include <unistd.h>

bool NO_FAIL(duckdb::CAPIResult &result) {
	if (result.HasError()) {
		fprintf(stderr, "Query failed with message: %s\n", result.ErrorMessage());
	}
	return result.success;
}

bool NO_FAIL(duckdb::unique_ptr<duckdb::CAPIResult> result) {
	return NO_FAIL(*result);
}

namespace duckdb {

template <>
bool CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_boolean(&result, col, row);
}

template <>
int8_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_int8(&result, col, row);
}

template <>
int16_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_int16(&result, col, row);
}

template <>
int32_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_int32(&result, col, row);
}

template <>
int64_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_int64(&result, col, row);
}

template <>
uint8_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_uint8(&result, col, row);
}

template <>
uint16_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_uint16(&result, col, row);
}

template <>
uint32_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_uint32(&result, col, row);
}

template <>
uint64_t CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_uint64(&result, col, row);
}

template <>
float CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_float(&result, col, row);
}

template <>
double CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_double(&result, col, row);
}

template <>
duckdb_decimal CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_decimal(&result, col, row);
}

template <>
duckdb_date CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_date *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
duckdb_time CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_time *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
duckdb_time_ns CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_time_ns *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
duckdb_timestamp CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_timestamp *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
duckdb_timestamp_s CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_timestamp_s *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
duckdb_timestamp_ms CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_timestamp_ms *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
duckdb_timestamp_ns CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_timestamp_ns *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
duckdb_interval CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_interval(&result, col, row);
}

template <>
duckdb_blob CAPIResult::Fetch(idx_t col, idx_t row) {
	auto data = (duckdb_blob *)duckdb_column_data(&result, col);
	return data[row];
}

template <>
string CAPIResult::Fetch(idx_t col, idx_t row) {
	auto value = duckdb_value_varchar(&result, col, row);
	string strval = value ? string(value) : string();
	free((void *)value);
	return strval;
}

template <>
duckdb_date_struct CAPIResult::Fetch(idx_t col, idx_t row) {
	auto value = duckdb_value_date(&result, col, row);
	return duckdb_from_date(value);
}

template <>
duckdb_time_struct CAPIResult::Fetch(idx_t col, idx_t row) {
	auto value = duckdb_value_time(&result, col, row);
	return duckdb_from_time(value);
}

template <>
duckdb_timestamp_struct CAPIResult::Fetch(idx_t col, idx_t row) {
	auto value = duckdb_value_timestamp(&result, col, row);
	return duckdb_from_timestamp(value);
}

template <>
duckdb_hugeint CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_hugeint(&result, col, row);
}

template <>
duckdb_uhugeint CAPIResult::Fetch(idx_t col, idx_t row) {
	return duckdb_value_uhugeint(&result, col, row);
}

// test_helpers.hpp

bool ApproxEqual(float ldecimal, float rdecimal) {
	if (std::isnan(ldecimal) && std::isnan(rdecimal)) {
		return true;
	}
	if (!std::isfinite(ldecimal) || !std::isfinite(rdecimal)) {
		return ldecimal == rdecimal;
	}
	float epsilon = std::fabs(rdecimal) * 0.01 + 0.00000001;
	return std::fabs(ldecimal - rdecimal) <= epsilon;
}

bool ApproxEqual(double ldecimal, double rdecimal) {
	if (std::isnan(ldecimal) && std::isnan(rdecimal)) {
		return true;
	}
	if (!std::isfinite(ldecimal) || !std::isfinite(rdecimal)) {
		return ldecimal == rdecimal;
	}
	double epsilon = std::fabs(rdecimal) * 0.01 + 0.00000001;
	return std::fabs(ldecimal - rdecimal) <= epsilon;
}

//! The directory the tests write their files in: $DUCKRS_CAPI_TEST_DIR, or duckrs_capi_test_dir.
string TestDirectoryPath() {
	const char *dir = getenv("DUCKRS_CAPI_TEST_DIR");
	string path = dir ? dir : "duckrs_capi_test_dir";
	mkdir(path.c_str(), 0755);
	return path;
}

string TestJoinPath(string path1, string path2) {
	return path1 + "/" + path2;
}

string TestCreatePath(string suffix) {
	return TestJoinPath(TestDirectoryPath(), suffix);
}

void TestDeleteFile(string path) {
	unlink(path.c_str());
}

void DeleteDatabase(string path) {
	TestDeleteFile(path);
	TestDeleteFile(path + ".wal");
}

void TestCreateDirectory(string path) {
	mkdir(path.c_str(), 0755);
}

void TestDeleteDirectory(string path) {
	string command = "rm -rf '" + path + "'";
	if (system(command.c_str()) != 0) {
		fprintf(stderr, "could not remove %s\n", path.c_str());
	}
}

//! `Date::ToString`: YYYY-MM-DD, with " (BC)" for years before 1.
Value Value::DATE(int32_t year, int32_t month, int32_t day) {
	char buffer[64];
	bool bc = year <= 0;
	snprintf(buffer, sizeof(buffer), "%04d-%02d-%02d%s", bc ? -year + 1 : year, month, day, bc ? " (BC)" : "");
	Value value;
	value.text = buffer;
	return value;
}

//! The fraction of a second of `Time::ToString`: `digits` digits, without the zeros it ends with, or none.
static string Fraction(int64_t fraction, int digits) {
	if (fraction == 0) {
		return "";
	}
	char buffer[32];
	snprintf(buffer, sizeof(buffer), ".%0*lld", digits, (long long)fraction);
	string text = buffer;
	while (!text.empty() && text.back() == '0') {
		text.pop_back();
	}
	return text;
}

Value Value::TIME(int32_t hour, int32_t minute, int32_t second, int32_t micros) {
	char buffer[64];
	snprintf(buffer, sizeof(buffer), "%02d:%02d:%02d", hour, minute, second);
	Value value;
	value.text = string(buffer) + Fraction(micros, 6);
	return value;
}

Value Value::TIME_NS(dtime_ns_t time) {
	int64_t nanos = time.value;
	int64_t seconds = nanos / 1000000000LL;
	char buffer[64];
	snprintf(buffer, sizeof(buffer), "%02lld:%02lld:%02lld", (long long)(seconds / 3600), (long long)(seconds / 60 % 60),
	         (long long)(seconds % 60));
	Value value;
	value.text = string(buffer) + Fraction(nanos % 1000000000LL, 9);
	return value;
}

} // namespace duckdb
