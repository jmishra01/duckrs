// duckrs shim: DuckDB's C++ header duckdb/common/numeric_utils.hpp is not used; the C API tests only need its
// NumericCast, which here is a plain static_cast (the tests cast values that fit).
#pragma once
#include "test_helpers.hpp"

namespace duckdb {

template <class TO, class FROM>
TO NumericCast(FROM value) {
	return static_cast<TO>(value);
}

//! `UnsafeNumericCast`: a plain static_cast too (arrow_string_view_type.hpp uses it)
template <class TO, class FROM>
TO UnsafeNumericCast(FROM value) {
	return static_cast<TO>(value);
}

} // namespace duckdb
