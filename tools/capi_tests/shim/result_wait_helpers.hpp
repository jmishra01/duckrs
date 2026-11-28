// pintail shim: the part of DuckDB's test/include/result_wait_helpers.hpp the C API tests use.
#pragma once
#include "test_helpers.hpp"

#include <chrono>

namespace duckdb {

//! Bounds a polling loop. Check Passed() on every iteration
struct Deadline {
	std::chrono::steady_clock::time_point expiry = std::chrono::steady_clock::now() + std::chrono::seconds(60);

	bool Passed() const {
		return std::chrono::steady_clock::now() >= expiry;
	}
};

} // namespace duckdb
