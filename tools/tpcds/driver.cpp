// Writes the TPC-DS tables as pipe-delimited text. It takes the place of dsdgen.cpp and
// append_info-c.cpp, which append to DuckDB tables, and runs the generators the same way:
// each table in order, its child tables along with it, on one thread.
#define DECLARER
#include "address.h"
#include "append_info.h"
#include "build_support.h"
#include "config.h"
#include "dist.h"
#include "genrand.h"
#include "init.h"
#include "params.h"
#include "parallel.h"
#include "porting.h"
#include "scaling.h"
#include "tdef_functions.h"
#include "tdefs.h"

#include <string>

static const int TABLE_COUNT = 24;

struct Output {
	FILE *file = nullptr;
	int first_column = 0;
	int64_t *null_bitmap = nullptr;
	bool first = true;
};

static Output outputs[TABLE_COUNT];

append_info *append_info_get(void *, int table_id) {
	return (append_info *)&outputs[table_id];
}

static bool is_null(Output *output, int column) {
	return (*output->null_bitmap & (int64_t(1) << (column - output->first_column))) != 0;
}

static void separator(Output *output) {
	if (!output->first) {
		fputc('|', output->file);
	}
	output->first = false;
}

void append_row_start(append_info info) {
	((Output *)info)->first = true;
}

void append_row_end(append_info info) {
	fputc('\n', ((Output *)info)->file);
}

void append_varchar(append_info info, const char *value, int column) {
	auto output = (Output *)info;
	separator(output);
	if (!is_null(output, column) && value) {
		fputs(value, output->file);
	}
}

void append_key(append_info info, int64_t value, int column) {
	auto output = (Output *)info;
	separator(output);
	if (!is_null(output, column) && value >= 0) {
		fprintf(output->file, "%lld", (long long)value);
	}
}

void append_integer(append_info info, int32_t value, int column) {
	auto output = (Output *)info;
	separator(output);
	if (!is_null(output, column)) {
		fprintf(output->file, "%d", value);
	}
}

void append_integer_decimal(append_info info, int32_t value, int column) {
	auto output = (Output *)info;
	separator(output);
	if (!is_null(output, column)) {
		fprintf(output->file, "%d.00", value);
	}
}

void append_boolean(append_info info, int32_t value, int column) {
	auto output = (Output *)info;
	separator(output);
	if (!is_null(output, column)) {
		fputs(value ? "true" : "false", output->file);
	}
}

// the value is a Julian date
void append_date(append_info info, int64_t value, int column) {
	auto output = (Output *)info;
	separator(output);
	if (is_null(output, column) || value < 0) {
		return;
	}
	// civil_from_days
	int64_t z = value - 2440588 + 719468;
	int64_t era = (z >= 0 ? z : z - 146096) / 146097;
	int64_t doe = z - era * 146097;
	int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
	int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	int64_t mp = (5 * doy + 2) / 153;
	int64_t day = doy - (153 * mp + 2) / 5 + 1;
	int64_t month = mp < 10 ? mp + 3 : mp - 9;
	int64_t year = yoe + era * 400 + (month <= 2 ? 1 : 0);
	fprintf(output->file, "%04lld-%02lld-%02lld", (long long)year, (long long)month, (long long)day);
}

void append_decimal(append_info info, decimal_t *value, int column) {
	auto output = (Output *)info;
	separator(output);
	if (is_null(output, column)) {
		return;
	}
	int64_t number = value->number;
	int64_t unit = 1;
	for (int i = 0; i < value->precision; i++) {
		unit *= 10;
	}
	if (number < 0) {
		fputc('-', output->file);
		number = -number;
	}
	fprintf(output->file, "%lld.%0*lld", (long long)(number / unit), value->precision, (long long)(number % unit));
}

int main(int argc, char **argv) {
	if (argc != 3) {
		fprintf(stderr, "usage: dsdgen <scale factor> <directory>\n");
		return 1;
	}
	std::string out = argv[2];
	InitConstants::Reset();
	resetCountCount();
	set_str("SCALE", argv[1]);
	init_rand();
	for (int table = 0; table < TABLE_COUNT; table++) {
		auto definition = getSimpleTdefsByNumber(table);
		outputs[table].file = fopen((out + "/" + definition->name + ".tbl").c_str(), "w");
		outputs[table].first_column = definition->nFirstColumn;
		outputs[table].null_bitmap = &definition->kNullBitMap;
	}
	for (int table = 0; table < TABLE_COUNT; table++) {
		auto definition = getSimpleTdefsByNumber(table);
		if (definition->flags & FL_CHILD) {
			continue;
		}
		ds_key_t rows = get_rowcount(table);
		if (definition->flags & FL_SMALL) {
			resetCountCount();
		}
		auto builder = getTdefFunctionsByNumber(table)->builder;
		for (ds_key_t row = 1; row <= rows; row++) {
			if (builder(nullptr, row)) {
				fprintf(stderr, "generating %s failed\n", definition->name);
				return 1;
			}
			row_stop(table);
		}
	}
	for (auto &output : outputs) {
		fclose(output.file);
	}
	return 0;
}
