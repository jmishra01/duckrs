#pragma once
#include <cstdio>
#include <string>
namespace duckdb {
struct StringUtil {
	template <class... A>
	static std::string Format(const char *format, A... arguments) {
		char text[1024];
		snprintf(text, sizeof(text), format, arguments...);
		return text;
	}
};
} // namespace duckdb
