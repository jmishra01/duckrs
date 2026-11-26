//! The sizes and field offsets of the structs of `duckrs_capi::types`, against those a C compiler gives the structs
//! of `duckdb.h`. The header is that of the DuckDB checkout (`DUCKDB_SOURCE`, or a `duckdb` directory next to the
//! repository or above it); without one, or without a C compiler, the test says so and passes.

use duckrs_capi::*;
use std::mem::{offset_of, size_of};
use std::path::{Path, PathBuf};
use std::process::Command;

fn duckdb_header() -> Option<PathBuf> {
    if let Ok(source) = std::env::var("DUCKDB_SOURCE") {
        return Some(Path::new(&source).join("src/include/duckdb.h"));
    }
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let header = dir.join("duckdb/src/include/duckdb.h");
        if header.is_file() {
            return Some(header);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// (C expression, the value Rust gives it)
fn layout() -> Vec<(String, usize)> {
    let mut checks = Vec::new();
    macro_rules! size {
        ($($t:ident),*) => {$(
            checks.push((format!("sizeof({})", stringify!($t)), size_of::<$t>()));
        )*};
    }
    macro_rules! offsets {
        ($t:ident: $($f:ident),*) => {$(
            checks.push((format!("offsetof({}, {})", stringify!($t), stringify!($f)), offset_of!($t, $f)));
        )*};
    }
    size!(
        duckdb_string_t, duckdb_date, duckdb_date_struct, duckdb_time, duckdb_time_struct, duckdb_time_ns,
        duckdb_time_tz, duckdb_time_tz_struct, duckdb_timestamp, duckdb_timestamp_struct, duckdb_timestamp_s,
        duckdb_timestamp_ms, duckdb_timestamp_ns, duckdb_interval, duckdb_hugeint, duckdb_uhugeint, duckdb_decimal,
        duckdb_query_progress_type, duckdb_list_entry, duckdb_column, duckdb_string, duckdb_blob, duckdb_bit,
        duckdb_bignum, duckdb_result, duckdb_type, duckdb_state, duckdb_error_type, duckdb_statement_type,
        duckdb_result_type, duckdb_pending_state, idx_t, sel_t
    );
    offsets!(duckdb_date_struct: year, month, day);
    offsets!(duckdb_time_struct: hour, min, sec, micros);
    offsets!(duckdb_time_tz_struct: time, offset);
    offsets!(duckdb_timestamp_struct: date, time);
    offsets!(duckdb_interval: months, days, micros);
    offsets!(duckdb_hugeint: lower, upper);
    offsets!(duckdb_uhugeint: lower, upper);
    offsets!(duckdb_decimal: width, scale, value);
    offsets!(duckdb_query_progress_type: percentage, rows_processed, total_rows_to_process);
    offsets!(duckdb_list_entry: offset, length);
    offsets!(duckdb_column: deprecated_data, deprecated_nullmask, deprecated_type, deprecated_name, internal_data);
    offsets!(duckdb_string: data, size);
    offsets!(duckdb_blob: data, size);
    offsets!(duckdb_bit: data, size);
    offsets!(duckdb_bignum: data, size, is_negative);
    offsets!(
        duckdb_result: deprecated_column_count,
        deprecated_row_count,
        deprecated_rows_changed,
        deprecated_columns,
        deprecated_error_message,
        internal_data
    );
    checks.push(("offsetof(duckdb_string_t, value.pointer.ptr)".into(), offset_of!(duckdb_string_t_pointer, ptr)));
    checks.push((
        "offsetof(duckdb_string_t, value.inlined.inlined)".into(),
        offset_of!(duckdb_string_t_inlined, inlined),
    ));
    checks
}

#[test]
fn structs_match_duckdb_h() {
    let Some(header) = duckdb_header().filter(|h| h.is_file()) else {
        eprintln!("no duckdb.h found (set DUCKDB_SOURCE): layout not checked");
        return;
    };
    let checks = layout();
    let dir = std::env::temp_dir().join(format!("duckrs_capi_layout_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut program = String::from("#include <stdio.h>\n#include <stddef.h>\n#include \"duckdb.h\"\nint main(void) {\n");
    for (expression, _) in &checks {
        program.push_str(&format!("\tprintf(\"%zu\\n\", (size_t)({expression}));\n"));
    }
    program.push_str("\treturn 0;\n}\n");
    let source = dir.join("layout.c");
    let binary = dir.join("layout");
    std::fs::write(&source, program).unwrap();
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let compiled = Command::new(&compiler)
        .arg("-I")
        .arg(header.parent().unwrap())
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .status();
    match compiled {
        Ok(status) if status.success() => {}
        _ => {
            eprintln!("could not compile the layout check with {compiler}: layout not checked");
            return;
        }
    }
    let output = Command::new(&binary).output().unwrap();
    let values: Vec<usize> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect();
    assert_eq!(values.len(), checks.len());
    let mismatches: Vec<String> = checks
        .iter()
        .zip(&values)
        .filter(|((_, rust), c)| rust != *c)
        .map(|((expression, rust), c)| format!("{expression}: C {c}, Rust {rust}"))
        .collect();
    std::fs::remove_dir_all(&dir).ok();
    assert!(mismatches.is_empty(), "layouts differ:\n{}", mismatches.join("\n"));
}
