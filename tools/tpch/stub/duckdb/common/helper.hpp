#pragma once
#include "duckdb/common/assert.hpp"
#include "duckdb/common/exception.hpp"
namespace duckdb { template<class T> T MinValue(T a, T b) { return a < b ? a : b; } template<class T> T MaxValue(T a, T b) { return a > b ? a : b; } }
