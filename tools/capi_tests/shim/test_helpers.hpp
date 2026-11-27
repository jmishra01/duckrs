//===----------------------------------------------------------------------===//
// duckrs: test_helpers.hpp for DuckDB's C API tests, without DuckDB's C++ internals.
//
// DuckDB's own test_helpers.hpp pulls in all of DuckDB. The C API tests use a small part of it: string helpers,
// the test directory, ApproxEqual, a few value types and REQUIRE_NO_FAIL. This header gives that part, written
// against the C API and the standard library only, so the tests link with libduckrs_capi alone.
//===----------------------------------------------------------------------===//

#pragma once

#include "catch.hpp"
#include "duckdb.h"

#include <algorithm>
#include <atomic>
#include <cassert>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <functional>
#include <limits>
#include <map>
#include <memory>
#include <set>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

#ifndef D_ASSERT
#define D_ASSERT(x) assert(x)
#endif

#ifndef STANDARD_VECTOR_SIZE
#define STANDARD_VECTOR_SIZE 2048
#endif

namespace duckdb {

//! `BaseAppender::DEFAULT_FLUSH_COUNT`
struct BaseAppender {
	static constexpr idx_t DEFAULT_FLUSH_COUNT = 204800;
};

using std::atomic;
using std::make_pair;
using std::move;
using std::pair;
using std::shared_ptr;
using std::string;
using std::to_string;
using std::unordered_map;
using std::unordered_set;

template <class T>
using vector = std::vector<T>;
template <class T, class D = std::default_delete<T>>
using unique_ptr = std::unique_ptr<T, D>;
template <class T>
using child_list_t = std::vector<std::pair<std::string, T>>;

template <class T, class... ARGS>
unique_ptr<T> make_uniq(ARGS &&...args) {
	return unique_ptr<T>(new T(std::forward<ARGS>(args)...));
}

template <class T, class... ARGS>
shared_ptr<T> make_shared_ptr(ARGS &&...args) {
	return std::make_shared<T>(std::forward<ARGS>(args)...);
}

template <class T>
struct NumericLimits {
	static constexpr T Minimum() {
		return std::numeric_limits<T>::lowest();
	}
	static constexpr T Maximum() {
		return std::numeric_limits<T>::max();
	}
};

bool ApproxEqual(float l, float r);
bool ApproxEqual(double l, double r);

string TestDirectoryPath();
string TestCreatePath(string suffix);
void TestDeleteFile(string path);
void DeleteDatabase(string path);
void TestCreateDirectory(string path);
void TestDeleteDirectory(string path);
string TestJoinPath(string path1, string path2);

//! `Printer::Print`: to stderr.
struct Printer {
	static void Print(const string &message) {
		fprintf(stderr, "%s\n", message.c_str());
	}
};

//! `StringUtil`: the helpers the C API tests use.
struct StringUtil {
	static bool Contains(const string &haystack, const string &needle) {
		return haystack.find(needle) != string::npos;
	}
	static bool StartsWith(const string &str, const string &prefix) {
		return str.rfind(prefix, 0) == 0;
	}
	static bool EndsWith(const string &str, const string &suffix) {
		return str.size() >= suffix.size() && str.compare(str.size() - suffix.size(), suffix.size(), suffix) == 0;
	}
	static string Lower(string str) {
		std::transform(str.begin(), str.end(), str.begin(), [](unsigned char c) { return std::tolower(c); });
		return str;
	}
	static string Upper(string str) {
		std::transform(str.begin(), str.end(), str.begin(), [](unsigned char c) { return std::toupper(c); });
		return str;
	}
	//! As DuckDB's: of C strings, so up to the first NUL
	static bool Equals(const char *l, const char *r) {
		return strcmp(l, r) == 0;
	}
	static bool Equals(const char *l, const string &r) {
		return Equals(l, r.c_str());
	}
	static bool Equals(const string &l, const string &r) {
		return Equals(l.c_str(), r.c_str());
	}
	static bool CIEquals(const string &l, const string &r) {
		return Lower(l) == Lower(r);
	}
	static string Repeat(const string &str, idx_t count) {
		string result;
		for (idx_t i = 0; i < count; i++) {
			result += str;
		}
		return result;
	}
	static vector<string> Split(const string &str, char delimiter) {
		vector<string> result;
		std::stringstream stream(str);
		string part;
		while (std::getline(stream, part, delimiter)) {
			result.push_back(part);
		}
		return result;
	}
	static string Join(const vector<string> &parts, const string &separator) {
		string result;
		for (size_t i = 0; i < parts.size(); i++) {
			if (i > 0) {
				result += separator;
			}
			result += parts[i];
		}
		return result;
	}
	//! `printf`-like, with every argument rendered as `operator<<` renders it (as DuckDB's Format does)
	template <class... ARGS>
	static string Format(const string &format, ARGS... args) {
		vector<string> rendered;
		Render(rendered, args...);
		string result;
		size_t next = 0;
		for (size_t i = 0; i < format.size(); i++) {
			if (format[i] != '%') {
				result += format[i];
				continue;
			}
			if (i + 1 < format.size() && format[i + 1] == '%') {
				result += '%';
				i++;
				continue;
			}
			// skip flags, width and length up to the conversion
			size_t j = i + 1;
			while (j < format.size() && !std::isalpha(static_cast<unsigned char>(format[j]))) {
				j++;
			}
			while (j < format.size() && (format[j] == 'l' || format[j] == 'h' || format[j] == 'z')) {
				j++;
			}
			if (next < rendered.size()) {
				result += rendered[next++];
			}
			i = j;
		}
		return result;
	}

private:
	static void Render(vector<string> &) {
	}
	template <class T, class... ARGS>
	static void Render(vector<string> &rendered, T first, ARGS... rest) {
		std::ostringstream stream;
		if constexpr (std::is_integral<T>::value) {
			// a uint8_t is a number to Format, not a character
			stream << +first;
		} else {
			stream << first;
		}
		rendered.push_back(stream.str());
		Render(rendered, rest...);
	}
};

struct hugeint_t {
	uint64_t lower = 0;
	int64_t upper = 0;
	hugeint_t() = default;
	hugeint_t(int64_t value) : lower(static_cast<uint64_t>(value)), upper(value < 0 ? -1 : 0) {
	}
	hugeint_t(int64_t upper_p, uint64_t lower_p) : lower(lower_p), upper(upper_p) {
	}
	static hugeint_t FromInt128(__int128 value) {
		return hugeint_t(static_cast<int64_t>(value >> 64), static_cast<uint64_t>(value));
	}
	__int128 ToInt128() const {
		return (static_cast<__int128>(upper) << 64) | static_cast<__int128>(lower);
	}
	hugeint_t operator-(const hugeint_t &rhs) const {
		return FromInt128(ToInt128() - rhs.ToInt128());
	}
	hugeint_t operator+(const hugeint_t &rhs) const {
		return FromInt128(ToInt128() + rhs.ToInt128());
	}
	hugeint_t operator*(const hugeint_t &rhs) const {
		return FromInt128(ToInt128() * rhs.ToInt128());
	}
	hugeint_t operator-() const {
		return FromInt128(-ToInt128());
	}
	bool operator==(const hugeint_t &rhs) const {
		return lower == rhs.lower && upper == rhs.upper;
	}
};

struct uhugeint_t {
	uint64_t lower = 0;
	uint64_t upper = 0;
	uhugeint_t() = default;
	uhugeint_t(uint64_t value) : lower(value), upper(0) {
	}
	uhugeint_t(uint64_t upper_p, uint64_t lower_p) : lower(lower_p), upper(upper_p) {
	}
};

struct Hugeint {
	static const hugeint_t POWERS_OF_TEN[39];
	static bool TryConvert(int64_t value, hugeint_t &result) {
		result = hugeint_t(value);
		return true;
	}
	template <class T>
	static hugeint_t Convert(T value) {
		return hugeint_t(static_cast<int64_t>(value));
	}
};

struct date_t {
	int32_t days = 0;
	date_t() = default;
	explicit date_t(int32_t days_p) : days(days_p) {
	}
};

struct dtime_t {
	int64_t micros = 0;
	dtime_t() = default;
	explicit dtime_t(int64_t micros_p) : micros(micros_p) {
	}
};

struct dtime_ns_t {
	int64_t value = 0;
	dtime_ns_t() = default;
	explicit dtime_ns_t(int64_t value_p) : value(value_p) {
	}
};

struct timestamp_t {
	int64_t value = 0;
	timestamp_t() = default;
	explicit timestamp_t(int64_t value_p) : value(value_p) {
	}
	static timestamp_t infinity() {
		return timestamp_t(std::numeric_limits<int64_t>::max());
	}
	static timestamp_t ninfinity() {
		return timestamp_t(-std::numeric_limits<int64_t>::max());
	}
};

struct Timestamp {
	static date_t GetDate(timestamp_t timestamp) {
		if (timestamp.value == timestamp_t::infinity().value) {
			return date_t(std::numeric_limits<int32_t>::max());
		}
		if (timestamp.value == timestamp_t::ninfinity().value) {
			return date_t(-std::numeric_limits<int32_t>::max());
		}
		const int64_t day = 86400000000LL;
		int64_t days = timestamp.value / day;
		if (timestamp.value % day < 0) {
			days--;
		}
		return date_t(static_cast<int32_t>(days));
	}
};

//! `Value`, for the few constants the tests compare results with: its text, rendered as DuckDB renders it.
class Value {
public:
	Value() = default;
	//! An INTEGER (`Value(int32_t)`), as the Arrow tests append them to a shim `DataChunk`
	explicit Value(int32_t value) : text(std::to_string(value)), integer(value) {
	}
	//! The value of an integer `Value`
	int64_t GetIntegerShim() const {
		return integer;
	}
	static Value DATE(int32_t year, int32_t month, int32_t day);
	static Value TIME(int32_t hour, int32_t minute, int32_t second, int32_t micros);
	static Value TIME_NS(dtime_ns_t time);
	string ToString() const {
		return text;
	}

private:
	string text;
	int64_t integer = 0;
};

} // namespace duckdb

#define REQUIRE_NO_FAIL(result) REQUIRE(NO_FAIL((result)))
#define REQUIRE_FAIL(result)    REQUIRE((result)->HasError())
