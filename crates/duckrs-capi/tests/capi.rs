//! The exported functions, called as C calls them.

use duckrs_capi::appender::*;
use duckrs_capi::database::*;
use duckrs_capi::datetime::*;
use duckrs_capi::helper::*;
use duckrs_capi::logical_type::*;
use duckrs_capi::prepared::*;
use duckrs_capi::result::*;
use duckrs_capi::value::*;
use duckrs_capi::vector::*;
use duckrs_capi::*;
use std::ffi::{CStr, CString, c_char};
use std::ptr::{null, null_mut};

fn text(ptr: *const c_char) -> String {
    assert!(!ptr.is_null());
    // SAFETY: the library returned a NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string()
}

/// A database and a connection to it, closed when dropped.
struct Db {
    db: duckdb_database,
    con: duckdb_connection,
}

impl Db {
    fn open() -> Self {
        let mut db = null_mut();
        let mut con = null_mut();
        // SAFETY: valid out-pointers.
        unsafe {
            assert_eq!(duckdb_open(null(), &mut db), DuckDBSuccess);
            assert_eq!(duckdb_connect(db, &mut con), DuckDBSuccess);
        }
        Self { db, con }
    }

    fn query(&self, sql: &str) -> (duckdb_state, duckdb_result) {
        let sql = CString::new(sql).unwrap();
        let mut result = duckdb_result::zeroed();
        // SAFETY: a live connection, a C string and an out-pointer.
        let state = unsafe { duckdb_query(self.con, sql.as_ptr(), &mut result) };
        (state, result)
    }

    fn ok(&self, sql: &str) {
        let (state, mut result) = self.query(sql);
        // SAFETY: the result the query wrote.
        unsafe {
            if state != DuckDBSuccess {
                panic!("{sql}: {}", text(duckdb_result_error(&mut result)));
            }
            duckdb_destroy_result(&mut result);
        }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // SAFETY: the handles `open` made.
        unsafe {
            duckdb_disconnect(&mut self.con);
            duckdb_close(&mut self.db);
        }
    }
}

#[test]
fn queries_and_the_deprecated_result() {
    let db = Db::open();
    db.ok("CREATE TABLE t (i INTEGER, s VARCHAR, d DECIMAL(9, 2))");
    let (state, mut result) = db.query("INSERT INTO t VALUES (1, 'one', 1.5), (NULL, 'two', NULL), (3, NULL, -2.25)");
    assert_eq!(state, DuckDBSuccess);
    // SAFETY: the results the queries wrote.
    unsafe {
        assert_eq!(duckdb_rows_changed(&mut result), 3);
        assert_eq!(duckdb_result_return_type(result), DUCKDB_RESULT_TYPE_CHANGED_ROWS);
        assert_eq!(duckdb_result_statement_type(result), DUCKDB_STATEMENT_TYPE_INSERT);
        duckdb_destroy_result(&mut result);

        let (state, mut result) = db.query("SELECT i, s, d FROM t ORDER BY i NULLS FIRST");
        assert_eq!(state, DuckDBSuccess);
        assert_eq!(duckdb_column_count(&mut result), 3);
        assert_eq!(duckdb_row_count(&mut result), 3);
        assert_eq!(text(duckdb_column_name(&mut result, 1)), "s");
        assert!(duckdb_column_name(&mut result, 3).is_null());
        assert_eq!(duckdb_column_type(&mut result, 2), DUCKDB_TYPE_DECIMAL);
        assert!(duckdb_value_is_null(&mut result, 0, 0));
        assert_eq!(duckdb_value_int32(&mut result, 0, 1), 1);
        assert_eq!(duckdb_value_int64(&mut result, 0, 2), 3);
        let s = duckdb_value_varchar(&mut result, 1, 1);
        assert_eq!(text(s), "one");
        duckdb_free(s.cast());
        assert!(duckdb_value_varchar(&mut result, 1, 2).is_null());
        assert_eq!(duckdb_value_double(&mut result, 2, 2), -2.25);
        // decimals round to integers as a cast does
        assert_eq!(duckdb_value_int8(&mut result, 2, 2), -2);
        let decimal = duckdb_value_decimal(&mut result, 2, 1);
        assert_eq!((decimal.width, decimal.scale, decimal.value.to_i128()), (9, 2, 150));
        let data = duckdb_column_data(&mut result, 0).cast::<i32>();
        assert_eq!(*data.add(2), 3);
        let nulls = duckdb_nullmask_data(&mut result, 0);
        assert!(*nulls && !*nulls.add(1));
        // the chunks are not for a result read the deprecated way
        assert!(duckdb_result_get_chunk(result, 0).is_null());
        duckdb_destroy_result(&mut result);

        let (state, mut result) = db.query("SELECT * FROM missing");
        assert_eq!(state, DuckDBError);
        assert!(text(duckdb_result_error(&mut result)).starts_with("Catalog Error: "));
        assert_eq!(duckdb_result_error_type(&mut result), DUCKDB_ERROR_CATALOG);
        duckdb_destroy_result(&mut result);
        // NULL handles
        assert_eq!(duckdb_column_count(null_mut()), 0);
        assert!(duckdb_result_error(null_mut()).is_null());
    }
}

#[test]
fn chunks_and_vectors() {
    let db = Db::open();
    let (state, mut result) = db.query("SELECT i, 'row ' || i AS s, [i, i + 1] AS l FROM range(3000) t(i)");
    assert_eq!(state, DuckDBSuccess);
    // SAFETY: the result the query wrote, and the chunks and vectors it gives.
    unsafe {
        let mut rows = 0;
        let mut chunks = 0;
        loop {
            let mut chunk = duckdb_fetch_chunk(result);
            if chunk.is_null() {
                break;
            }
            chunks += 1;
            let size = duckdb_data_chunk_get_size(chunk) as usize;
            let ints = duckdb_vector_get_data(duckdb_data_chunk_get_vector(chunk, 0)).cast::<i64>();
            let strings = duckdb_vector_get_data(duckdb_data_chunk_get_vector(chunk, 1)).cast::<duckdb_string_t>();
            let list = duckdb_data_chunk_get_vector(chunk, 2);
            let entries = duckdb_vector_get_data(list).cast::<duckdb_list_entry>();
            let child = duckdb_vector_get_data(duckdb_list_vector_get_child(list)).cast::<i64>();
            for row in 0..size {
                let i = *ints.add(row);
                assert_eq!(i as usize, rows + row);
                let string = strings.add(row);
                let length = duckdb_string_t_length(*string) as usize;
                let bytes = std::slice::from_raw_parts(duckdb_string_t_data(string).cast::<u8>(), length);
                assert_eq!(bytes, format!("row {i}").as_bytes());
                let entry = *entries.add(row);
                assert_eq!(entry.length, 2);
                assert_eq!(*child.add(entry.offset as usize + 1), i + 1);
            }
            rows += size;
            duckdb_destroy_data_chunk(&mut chunk);
        }
        assert_eq!((rows, chunks), (3000, 2));
        // the deprecated functions are not for a result read a chunk at a time
        assert!(duckdb_column_data(&mut result, 0).is_null());
        duckdb_destroy_result(&mut result);

        // a chunk made in C, with a NULL
        let mut ty = duckdb_create_logical_type(DUCKDB_TYPE_INTEGER);
        let mut chunk = duckdb_create_data_chunk(&mut ty, 1);
        let vector = duckdb_data_chunk_get_vector(chunk, 0);
        assert!(duckdb_vector_get_validity(vector).is_null());
        duckdb_vector_ensure_validity_writable(vector);
        let validity = duckdb_vector_get_validity(vector);
        duckdb_validity_set_row_invalid(validity, 1);
        assert!(!duckdb_validity_row_is_valid(validity, 1) && duckdb_validity_row_is_valid(validity, 0));
        *duckdb_vector_get_data(vector).cast::<i32>() = 7;
        duckdb_data_chunk_set_size(chunk, 2);
        assert_eq!(duckdb_data_chunk_get_size(chunk), 2);
        duckdb_destroy_data_chunk(&mut chunk);
        duckdb_destroy_logical_type(&mut ty);
    }
}

#[test]
fn prepared_statements() {
    let db = Db::open();
    db.ok("CREATE TABLE t AS SELECT i FROM range(10) r(i)");
    let mut stmt = null_mut();
    let sql = CString::new("SELECT count(*) FROM t WHERE i >= $low AND i < $high").unwrap();
    // SAFETY: the statement `duckdb_prepare` makes, and the results it gives.
    unsafe {
        assert_eq!(duckdb_prepare(db.con, sql.as_ptr(), &mut stmt), DuckDBSuccess);
        assert_eq!(duckdb_nparams(stmt), 2);
        let name = duckdb_parameter_name(stmt, 2);
        assert_eq!(text(name), "high");
        duckdb_free(name.cast_mut().cast());
        let mut index = 0;
        let high = CString::new("HIGH").unwrap();
        assert_eq!(
            duckdb_bind_parameter_index(stmt, &mut index, high.as_ptr()),
            DuckDBSuccess
        );
        assert_eq!(index, 2);
        assert_eq!(duckdb_bind_int32(stmt, 1, 3), DuckDBSuccess);
        assert_eq!(duckdb_bind_int64(stmt, 2, 7), DuckDBSuccess);
        assert_eq!(duckdb_bind_int64(stmt, 3, 7), DuckDBError);
        assert!(text(duckdb_prepare_error(stmt)).contains("Can not bind to parameter number 3"));
        let mut result = duckdb_result::zeroed();
        assert_eq!(duckdb_execute_prepared(stmt, &mut result), DuckDBSuccess);
        assert_eq!(duckdb_value_int64(&mut result, 0, 0), 4);
        duckdb_destroy_result(&mut result);
        assert_eq!(duckdb_prepared_statement_type(stmt), DUCKDB_STATEMENT_TYPE_SELECT);
        duckdb_destroy_prepare(&mut stmt);
        assert!(stmt.is_null());

        let bad = CString::new("SELECT * FROM nope").unwrap();
        assert_eq!(duckdb_prepare(db.con, bad.as_ptr(), &mut stmt), DuckDBError);
        assert!(text(duckdb_prepare_error(stmt)).contains("nope"));
        duckdb_destroy_prepare(&mut stmt);
    }
}

#[test]
fn appenders() {
    let db = Db::open();
    db.ok("CREATE TABLE t (i INTEGER, s VARCHAR, d DOUBLE DEFAULT 2.5)");
    let mut appender = null_mut();
    let table = CString::new("t").unwrap();
    // SAFETY: the appender `duckdb_appender_create` makes.
    unsafe {
        assert_eq!(
            duckdb_appender_create(db.con, null(), table.as_ptr(), &mut appender),
            DuckDBSuccess
        );
        assert_eq!(duckdb_appender_column_count(appender), 3);
        for i in 0..5000 {
            let s = CString::new(format!("v{i}")).unwrap();
            assert_eq!(duckdb_append_int32(appender, i), DuckDBSuccess);
            assert_eq!(duckdb_append_varchar(appender, s.as_ptr()), DuckDBSuccess);
            assert_eq!(duckdb_append_default(appender), DuckDBSuccess);
            assert_eq!(duckdb_appender_end_row(appender), DuckDBSuccess);
        }
        assert_eq!(duckdb_append_int64(appender, 1), DuckDBSuccess);
        assert_eq!(duckdb_appender_end_row(appender), DuckDBError);
        assert_eq!(
            text(duckdb_appender_error(appender)),
            "Call to EndRow before all columns have been appended to!"
        );
        assert_eq!(duckdb_append_null(appender), DuckDBSuccess);
        assert_eq!(duckdb_append_double(appender, 1.0), DuckDBSuccess);
        assert_eq!(duckdb_append_null(appender), DuckDBError);
        assert_eq!(text(duckdb_appender_error(appender)), "Too many appends for chunk!");
        assert_eq!(duckdb_appender_end_row(appender), DuckDBSuccess);
        assert_eq!(duckdb_appender_destroy(&mut appender), DuckDBSuccess);
    }
    let (_, mut result) = db.query("SELECT count(*), sum(i), sum(d), count(s) FROM t");
    // SAFETY: the result the query wrote.
    unsafe {
        assert_eq!(duckdb_value_int64(&mut result, 0, 0), 5001);
        assert_eq!(duckdb_value_int64(&mut result, 1, 0), 12_497_501);
        assert_eq!(duckdb_value_double(&mut result, 2, 0), 12_501.0);
        assert_eq!(duckdb_value_int64(&mut result, 3, 0), 5000);
        duckdb_destroy_result(&mut result);
        let missing = CString::new("missing").unwrap();
        assert_eq!(
            duckdb_appender_create(db.con, null(), missing.as_ptr(), &mut appender),
            DuckDBError
        );
        assert!(!duckdb_appender_error(appender).is_null());
        duckdb_appender_destroy(&mut appender);
    }
}

#[test]
fn values_types_and_dates() {
    // SAFETY: the values and types the library makes.
    unsafe {
        let mut value = duckdb_create_int64(-42);
        assert_eq!(duckdb_get_int32(value), -42);
        assert_eq!(duckdb_get_uint8(value), 0);
        let s = duckdb_get_varchar(value);
        assert_eq!(text(s), "-42");
        duckdb_free(s.cast());
        assert_eq!(duckdb_get_type_id(duckdb_get_value_type(value)), DUCKDB_TYPE_BIGINT);
        duckdb_destroy_value(&mut value);

        let mut child = duckdb_create_logical_type(DUCKDB_TYPE_VARCHAR);
        let mut list = duckdb_create_list_type(child);
        assert_eq!(duckdb_get_type_id(list), DUCKDB_TYPE_LIST);
        let mut list_child = duckdb_list_type_child_type(list);
        assert_eq!(duckdb_get_type_id(list_child), DUCKDB_TYPE_VARCHAR);
        let mut decimal = duckdb_create_decimal_type(18, 3);
        assert_eq!((duckdb_decimal_width(decimal), duckdb_decimal_scale(decimal)), (18, 3));
        assert_eq!(duckdb_decimal_internal_type(decimal), DUCKDB_TYPE_BIGINT);
        assert!(duckdb_create_decimal_type(40, 3).is_null());
        for ty in [&mut child, &mut list, &mut list_child, &mut decimal] {
            duckdb_destroy_logical_type(ty);
        }

        let date = duckdb_to_date(duckdb_date_struct {
            year: 2024,
            month: 2,
            day: 29,
        });
        assert_eq!(date.days, 19782);
        assert_eq!(
            duckdb_from_date(date),
            duckdb_date_struct {
                year: 2024,
                month: 2,
                day: 29
            }
        );
        assert_eq!(
            duckdb_to_date(duckdb_date_struct {
                year: 2023,
                month: 2,
                day: 29
            })
            .days,
            0
        );
        let time = duckdb_from_time(duckdb_time { micros: 3_723_000_004 });
        assert_eq!((time.hour, time.min, time.sec, time.micros), (1, 2, 3, 4));
        assert_eq!(duckdb_hugeint_to_double(duckdb_double_to_hugeint(-1e20)), -1e20);
        let d = duckdb_double_to_decimal(12.345, 9, 2);
        assert_eq!(d.value.to_i128(), 1235);
        assert_eq!(duckdb_decimal_to_double(d), 12.35);
    }
    assert_eq!(duckdb_vector_size(), 2048);
    assert!(text(duckdb_library_version()).starts_with('v'));
    assert!(duckdb_config_count() > 0);
}
