#pragma once
#include <cstdio>
#include <cstdlib>
#include <string>
namespace duckdb {
struct Failure {
	template <class... A>
	explicit Failure(const char *message, A...) {
		fprintf(stderr, "%s\n", message);
		abort();
	}
	template <class... A>
	explicit Failure(const std::string &message, A...) : Failure(message.c_str()) {
	}
};
using InternalException = Failure;
using InvalidInputException = Failure;
using OutOfMemoryException = Failure;
} // namespace duckdb
