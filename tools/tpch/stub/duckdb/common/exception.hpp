#pragma once
#include <cstdio>
#include <cstdlib>
#include <cstdint>
namespace duckdb {
typedef uint64_t idx_t;
struct InternalException { template<class... A> InternalException(const char *m, A...) { fprintf(stderr, "%s\n", m); abort(); } };
}
