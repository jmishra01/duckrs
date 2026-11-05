// Scratch driver: DuckDB's dbgen generators, writing pipe-delimited text.
#include "dbgen/dbgen_gunk.hpp"
#include "duckdb/common/exception.hpp"
#include "duckdb/common/helper.hpp"
#define DECLARER
#include "dbgen/dss.h"
#include "dbgen/rng64.h"
#include "dbgen/dsstypes.h"
#include <cstring>
#include <string>
#include <vector>
#include <cmath>
using namespace duckdb;
template <class T, class S> T NumericCast(S v) { return (T)v; }
namespace tpch {
struct string_t {
	std::string *s;
	char *GetDataWriteable() { return s->data(); }
	void Finalize() {}
};
struct tpch_append_information {
	FILE *f = nullptr;
	std::vector<std::string> cols;
	string_t slot;
};
static void append_begin_row(tpch_append_information &i) { i.cols.clear(); }
static void append_end_row(tpch_append_information &i) {
	for (size_t c = 0; c < i.cols.size(); c++) {
		if (c) fputc('|', i.f);
		fwrite(i.cols[c].data(), 1, i.cols[c].size(), i.f);
	}
	fputc('\n', i.f);
}
void append_int32(tpch_append_information &i, int32_t v) { i.cols.push_back(std::to_string(v)); }
void append_int64(tpch_append_information &i, int64_t v) { i.cols.push_back(std::to_string(v)); }
void append_string_reference(tpch_append_information &i, const char *v, idx_t l) { i.cols.emplace_back(v, l); }
void append_decimal(tpch_append_information &i, int64_t v) {
	char b[40];
	snprintf(b, 40, "%s%lld.%02lld", v < 0 ? "-" : "", (long long)(llabs(v) / 100), (long long)(llabs(v) % 100));
	i.cols.push_back(b);
}
void append_char(tpch_append_information &i, char v) { i.cols.push_back(std::string(1, v)); }
void append_date(tpch_append_information &i, DSS_HUGE value) {
	long z = (long)(value - STARTDATE + 8035) + 719468;
	long era = (z >= 0 ? z : z - 146096) / 146097;
	long doe = z - era * 146097;
	long yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
	long y = yoe + era * 400;
	long doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	long mp = (5 * doy + 2) / 153;
	long d = doy - (153 * mp + 2) / 5 + 1;
	long m = mp + (mp < 10 ? 3 : -9);
	char b[20];
	snprintf(b, 20, "%04ld-%02ld-%02ld", y + (m <= 2), m, d);
	i.cols.push_back(b);
}
static string_t &append_empty_string(tpch_append_information &i, idx_t length) {
	i.cols.emplace_back(length, '\0');
	i.slot.s = &i.cols.back();
	return i.slot;
}
