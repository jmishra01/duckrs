// pintail shim: DuckDB's C++ header duckdb/common/arrow/arrow_converter.hpp is not used. The C API tests call
// `ArrowConverter::ToArrowSchema`, which here goes through `duckdb_to_arrow_schema` with the arrow options of the
// shim's ClientProperties (see arrow_appender.hpp).
#pragma once
#include "duckdb/common/arrow/arrow_appender.hpp"

namespace duckdb {

struct ArrowConverter {
	static void ToArrowSchema(ArrowSchema *out_schema, const vector<LogicalType> &types, const vector<string> &names,
	                          const ClientProperties &options) {
		vector<duckdb_logical_type> c_types;
		vector<const char *> c_names;
		for (idx_t i = 0; i < types.size(); i++) {
			c_types.push_back(duckdb_create_logical_type(types[i].CType()));
			c_names.push_back(names[i].c_str());
		}
		auto error = duckdb_to_arrow_schema(options.arrow_options.get(), c_types.data(), c_names.data(),
		                                    c_types.size(), out_schema);
		for (auto &type : c_types) {
			duckdb_destroy_logical_type(&type);
		}
		if (error) {
			string message = duckdb_error_data_message(error);
			duckdb_destroy_error_data(&error);
			throw std::runtime_error(message);
		}
	}
};

} // namespace duckdb
