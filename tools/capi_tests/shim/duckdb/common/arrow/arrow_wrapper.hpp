// pintail shim: DuckDB's C++ header duckdb/common/arrow/arrow_wrapper.hpp is not used. The C API tests need the Arrow C
// data interface structs of duckdb/common/arrow/arrow.hpp (a plain header, used as it is) and ArrowSchemaWrapper,
// which releases the schema it holds when it goes away.
#pragma once
#include "duckdb/common/arrow/arrow.hpp"
#include "test_helpers.hpp"

namespace duckdb {

//! `ArrowSchemaWrapper`: owns an ArrowSchema and releases it on destruction.
class ArrowSchemaWrapper {
public:
	ArrowSchema arrow_schema;

	ArrowSchemaWrapper() {
		arrow_schema.Init();
	}
	~ArrowSchemaWrapper() {
		if (arrow_schema.release) {
			arrow_schema.release(&arrow_schema);
		}
	}
	ArrowSchemaWrapper(const ArrowSchemaWrapper &) = delete;
	ArrowSchemaWrapper &operator=(const ArrowSchemaWrapper &) = delete;
};

} // namespace duckdb
