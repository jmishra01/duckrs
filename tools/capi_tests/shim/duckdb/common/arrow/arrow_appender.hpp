// pintail shim: DuckDB's C++ header duckdb/common/arrow/arrow_appender.hpp is not used. The C API tests build an Arrow
// array of a few chunks of fixed-width values with `ArrowAppender`; here that is a small builder over a shim
// `DataChunk` (values appended as `Value`s), giving a struct array whose release callback frees its buffers.
// `ClientProperties` holds the arrow options of a connection (`duckdb_connection_get_arrow_options`).
#pragma once
#include "duckdb/common/arrow/arrow_wrapper.hpp"
#include "test_helpers.hpp"

#include <array>

namespace duckdb {

//! `LogicalTypeId`, as far as the tests use it
enum class LogicalTypeId : uint8_t { INTEGER, BIGINT };

//! `LogicalType` of a fixed-width type, with the C API type it is
class LogicalType {
public:
	explicit LogicalType(LogicalTypeId id_p) : type_id(id_p) {
	}
	LogicalTypeId id() const {
		return type_id;
	}
	duckdb_type CType() const {
		switch (type_id) {
		case LogicalTypeId::INTEGER:
			return DUCKDB_TYPE_INTEGER;
		case LogicalTypeId::BIGINT:
			return DUCKDB_TYPE_BIGINT;
		}
		return DUCKDB_TYPE_INVALID;
	}
	idx_t Width() const {
		return type_id == LogicalTypeId::INTEGER ? 4 : 8;
	}

private:
	LogicalTypeId type_id;
};

//! `ClientProperties`: the arrow options of a connection
struct ClientProperties {
	std::shared_ptr<_duckdb_arrow_options> arrow_options;
};

//! `ClientContext`, for `Connection::context->GetClientProperties()`: the default arrow options, read from a
//! connection to a database of its own (the tests read them from a connection with no settings changed).
struct ShimClientContext {
	static ClientProperties GetClientProperties() {
		duckdb_database database = nullptr;
		duckdb_connection connection = nullptr;
		duckdb_arrow_options options = nullptr;
		if (duckdb_open(nullptr, &database) == DuckDBSuccess && duckdb_connect(database, &connection) == DuckDBSuccess) {
			duckdb_connection_get_arrow_options(connection, &options);
		}
		duckdb_disconnect(&connection);
		duckdb_close(&database);
		ClientProperties result;
		result.arrow_options = std::shared_ptr<_duckdb_arrow_options>(
		    options, [](duckdb_arrow_options options) { duckdb_destroy_arrow_options(&options); });
		return result;
	}
};

//! `ArrowTypeExtensionData`: the tests only name it in an empty map
struct ArrowTypeExtensionData {};

struct Allocator {};

//! `Vector` of a shim `DataChunk`: the integers appended to it
class Vector {
public:
	explicit Vector(LogicalType type_p) : type(type_p) {
	}
	void Append(const Value &value) {
		values.push_back(value.GetIntegerShim());
	}
	LogicalType type;
	std::vector<int64_t> values;
};

//! `DataChunk` of fixed-width columns
class DataChunk {
public:
	void Initialize(Allocator &, const vector<LogicalType> &types, idx_t) {
		data.clear();
		for (auto &type : types) {
			data.emplace_back(type);
		}
	}
	void SetChildCardinality(idx_t count_p) {
		count = count_p;
	}
	idx_t size() const {
		return count;
	}
	vector<Vector> data;

private:
	idx_t count = 0;
};

//! `ArrowAppender` of fixed-width columns without NULLs
class ArrowAppender {
public:
	ArrowAppender(vector<LogicalType> types_p, idx_t, ClientProperties,
	              const unordered_map<idx_t, const shared_ptr<ArrowTypeExtensionData>> &)
	    : types(std::move(types_p)), columns(types.size()) {
	}

	void Append(DataChunk &input, idx_t from, idx_t to, idx_t) {
		for (idx_t col = 0; col < types.size(); col++) {
			auto &values = input.data[col].values;
			for (idx_t row = from; row < to; row++) {
				columns[col].push_back(values[row]);
			}
		}
		row_count += to - from;
	}

	ArrowArray Finalize() {
		auto holder = new Holder();
		holder->children.resize(types.size());
		holder->child_ptrs.resize(types.size());
		holder->child_buffers.resize(types.size());
		holder->data.resize(types.size());
		for (idx_t col = 0; col < types.size(); col++) {
			auto width = types[col].Width();
			auto &bytes = holder->data[col];
			bytes.resize(row_count * width);
			for (idx_t row = 0; row < row_count; row++) {
				if (width == 4) {
					auto value = static_cast<int32_t>(columns[col][row]);
					memcpy(&bytes[row * width], &value, width);
				} else {
					memcpy(&bytes[row * width], &columns[col][row], width);
				}
			}
			holder->child_buffers[col][0] = nullptr;
			holder->child_buffers[col][1] = bytes.data();
			auto &child = holder->children[col];
			child.Init();
			child.length = static_cast<int64_t>(row_count);
			child.n_buffers = 2;
			child.buffers = holder->child_buffers[col].data();
			child.children = nullptr;
			child.dictionary = nullptr;
			child.release = ReleaseChild;
			holder->child_ptrs[col] = &child;
		}
		ArrowArray result;
		result.Init();
		result.length = static_cast<int64_t>(row_count);
		result.n_buffers = 1;
		result.buffers = holder->parent_buffers;
		result.n_children = static_cast<int64_t>(types.size());
		result.children = holder->child_ptrs.data();
		result.dictionary = nullptr;
		result.release = ReleaseParent;
		result.private_data = holder;
		return result;
	}

private:
	struct Holder {
		const void *parent_buffers[1] = {nullptr};
		std::vector<ArrowArray> children;
		std::vector<ArrowArray *> child_ptrs;
		std::vector<std::array<const void *, 2>> child_buffers;
		std::vector<std::vector<uint8_t>> data;
	};
	static void ReleaseChild(ArrowArray *array) {
		array->release = nullptr;
	}
	static void ReleaseParent(ArrowArray *array) {
		for (int64_t i = 0; i < array->n_children; i++) {
			if (array->children[i]->release) {
				array->children[i]->release(array->children[i]);
			}
		}
		delete static_cast<Holder *>(array->private_data);
		array->private_data = nullptr;
		array->release = nullptr;
	}

	vector<LogicalType> types;
	vector<std::vector<int64_t>> columns;
	idx_t row_count = 0;
};

} // namespace duckdb
