//! Replacement scans and table descriptions, called as C calls them.

use duckrs_capi::database::*;
use duckrs_capi::helper::duckdb_free;
use duckrs_capi::logical_type::*;
use duckrs_capi::replacement_scan::*;
use duckrs_capi::result::*;
use duckrs_capi::table_description::*;
use duckrs_capi::value::*;
use duckrs_capi::*;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};

fn text(ptr: *const c_char) -> String {
    assert!(!ptr.is_null());
    // SAFETY: the library returned a NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string()
}

fn open() -> duckdb_database {
    let mut db = null_mut();
    // SAFETY: a valid out-pointer.
    assert_eq!(unsafe { duckdb_open(null(), &mut db) }, DuckDBSuccess);
    db
}

fn connect(db: duckdb_database) -> duckdb_connection {
    let mut con = null_mut();
    // SAFETY: a live database and a valid out-pointer.
    assert_eq!(unsafe { duckdb_connect(db, &mut con) }, DuckDBSuccess);
    con
}

/// The rows of the first column of `sql` as BIGINTs, or the error.
fn query(con: duckdb_connection, sql: &str) -> Result<Vec<i64>, String> {
    let sql = CString::new(sql).unwrap();
    let mut result = duckdb_result::zeroed();
    // SAFETY: a live connection, a C string and an out-pointer; the result is destroyed after it is read.
    unsafe {
        let state = duckdb_query(con, sql.as_ptr(), &mut result);
        let rows = if state == DuckDBSuccess {
            Ok((0..duckdb_row_count(&mut result))
                .map(|row| duckdb_value_int64(&mut result, 0, row))
                .collect())
        } else {
            Err(text(duckdb_result_error(&mut result)))
        };
        duckdb_destroy_result(&mut result);
        rows
    }
}

/// The extra data of `number_scanner`, and how often it was freed.
struct BaseNumber {
    number: i64,
}

static FREED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn destroy_base_number(data: *mut c_void) {
    // SAFETY: the box `number_scans` gave as the extra data.
    drop(unsafe { Box::from_raw(data.cast::<BaseNumber>()) });
    FREED.fetch_add(1, Ordering::SeqCst);
}

/// `range(name + base)` for a table named by a number.
unsafe extern "C" fn number_scanner(info: duckdb_replacement_scan_info, table_name: *const c_char, data: *mut c_void) {
    let Ok(number) = text(table_name).parse::<i64>() else {
        return;
    };
    // SAFETY: the extra data is the `BaseNumber` registered with the scan; the info is the callback's.
    unsafe {
        let base = &*data.cast::<BaseNumber>();
        duckdb_replacement_scan_set_function_name(info, c"range".as_ptr());
        let mut value = duckdb_create_int64(number + base.number);
        duckdb_replacement_scan_add_parameter(info, value);
        duckdb_destroy_value(&mut value);
    }
}

unsafe extern "C" fn error_scanner(info: duckdb_replacement_scan_info, _table_name: *const c_char, _data: *mut c_void) {
    // SAFETY: the info is the callback's; null arguments are ignored.
    unsafe {
        duckdb_replacement_scan_set_error(null_mut(), null());
        duckdb_replacement_scan_set_error(info, null());
        duckdb_replacement_scan_set_error(info, c"error in replacement scan".as_ptr());
    }
}

#[test]
fn replacement_scans_are_on_the_database() {
    let mut db = open();
    // a connection made before the scan sees it too (`DBConfig::replacement_scans`)
    let mut early = connect(db);
    let base = Box::into_raw(Box::new(BaseNumber { number: 3 }));
    // SAFETY: a live database, the callbacks above and the extra data they read.
    unsafe { duckdb_add_replacement_scan(db, Some(number_scanner), base.cast(), Some(destroy_base_number)) };
    let mut late = connect(db);
    assert_eq!(query(early, "SELECT * FROM \"2\"").unwrap(), vec![0, 1, 2, 3, 4]);
    // SAFETY: the extra data is C's to change between queries.
    unsafe { (*base).number = 1 };
    assert_eq!(query(late, "SELECT * FROM \"2\"").unwrap(), vec![0, 1, 2]);
    // the table's name is the alias, and an alias given takes its place
    assert_eq!(query(late, "SELECT \"1\".range FROM \"1\"").unwrap(), vec![0, 1]);
    assert_eq!(query(late, "SELECT x.r FROM \"1\" AS x(r)").unwrap(), vec![0, 1]);
    // a table of the catalog comes first
    query(late, "CREATE TABLE \"5\" AS SELECT 42 AS i").unwrap();
    assert_eq!(query(early, "SELECT * FROM \"5\"").unwrap(), vec![42]);
    let error = query(late, "SELECT * FROM nonexistant").unwrap_err();
    assert!(error.contains("nonexistant"), "{error}");
    assert_eq!(FREED.load(Ordering::SeqCst), 0);
    // SAFETY: the handles made above; closing the database frees the extra data.
    unsafe {
        duckdb_disconnect(&mut early);
        duckdb_disconnect(&mut late);
        duckdb_close(&mut db);
    }
    assert_eq!(FREED.load(Ordering::SeqCst), 1);
}

#[test]
fn replacement_scan_errors() {
    let mut db = open();
    // SAFETY: a live database and the callback above, without extra data; a null callback or database is ignored.
    unsafe {
        duckdb_add_replacement_scan(db, None, null_mut(), None);
        duckdb_add_replacement_scan(null_mut(), Some(error_scanner), null_mut(), None);
        duckdb_add_replacement_scan(db, Some(error_scanner), null_mut(), None);
    }
    let mut con = connect(db);
    assert_eq!(
        query(con, "SELECT * FROM nonexistant").unwrap_err(),
        "Binder Error: Error in replacement scan: error in replacement scan\n"
    );
    // SAFETY: the handles made above.
    unsafe {
        duckdb_disconnect(&mut con);
        duckdb_close(&mut db);
    }
}

#[test]
fn table_descriptions() {
    let mut db = open();
    let mut con = connect(db);
    query(con, "CREATE TABLE t (i INTEGER, j VARCHAR DEFAULT 'x', k AS (i + 1))").unwrap();
    query(con, "CREATE VIEW v AS SELECT 1 AS i").unwrap();
    // SAFETY: live handles, C strings and out-pointers; every description is destroyed.
    unsafe {
        let mut description = null_mut();
        assert_eq!(
            duckdb_table_description_create(con, null(), c"v".as_ptr(), &mut description),
            DuckDBError
        );
        assert_eq!(
            text(duckdb_table_description_error(description)),
            "No table with that schema+name could be located"
        );
        duckdb_table_description_destroy(&mut description);
        assert!(description.is_null());

        assert_eq!(
            duckdb_table_description_create_ext(con, c"nope".as_ptr(), null(), c"t".as_ptr(), &mut description),
            DuckDBError
        );
        assert!(!duckdb_table_description_error(description).is_null());
        duckdb_table_description_destroy(&mut description);
        assert_eq!(
            duckdb_table_description_create(null_mut(), null(), c"t".as_ptr(), null_mut()),
            DuckDBError
        );

        assert_eq!(
            duckdb_table_description_create(con, null(), c"t".as_ptr(), &mut description),
            DuckDBSuccess
        );
        assert!(duckdb_table_description_error(description).is_null());
        assert_eq!(duckdb_table_description_get_column_count(description), 3);
        let mut has_default = true;
        assert_eq!(
            duckdb_column_has_default(description, 0, &mut has_default),
            DuckDBSuccess
        );
        assert!(!has_default);
        assert_eq!(
            duckdb_column_has_default(description, 1, &mut has_default),
            DuckDBSuccess
        );
        assert!(has_default);
        assert_eq!(
            duckdb_column_has_default(description, 2, &mut has_default),
            DuckDBSuccess
        );
        assert!(!has_default);
        assert_eq!(duckdb_column_has_default(description, 3, &mut has_default), DuckDBError);
        assert_eq!(
            text(duckdb_table_description_error(description)),
            "Column index 3 is out of range, table only has 3 columns"
        );
        assert_eq!(duckdb_column_has_default(description, 0, null_mut()), DuckDBError);
        assert_eq!(
            text(duckdb_table_description_error(description)),
            "Please provide a valid (non-null) 'out' variable"
        );
        let name = duckdb_table_description_get_column_name(description, 2);
        assert_eq!(text(name), "k");
        duckdb_free(name.cast());
        let mut ty = duckdb_table_description_get_column_type(description, 1);
        assert_eq!(duckdb_get_type_id(ty), DUCKDB_TYPE_VARCHAR);
        duckdb_destroy_logical_type(&mut ty);
        assert!(duckdb_table_description_get_column_type(description, 3).is_null());
        duckdb_table_description_destroy(&mut description);
        duckdb_disconnect(&mut con);
        duckdb_close(&mut db);
    }
}
