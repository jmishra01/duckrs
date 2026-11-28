//! Renders generated chunks in the pipe-delimited `.tbl` format of `tools/tpch/driver_*.cpp`.

use pintail_common::types::{Date, LogicalTypeId};
use pintail_common::vector::DataChunk;
use std::io::Write;

/// Appends the rows of `chunk` to `out`: columns separated by `|`, rows ended by `\n`, decimals with
/// two digits, dates as `yyyy-mm-dd`.
pub fn write_chunk(out: &mut Vec<u8>, chunk: &DataChunk) {
    let columns = chunk.columns();
    for row in 0..chunk.len() {
        for (c, column) in columns.iter().enumerate() {
            if c > 0 {
                out.push(b'|');
            }
            match column.logical_type().id() {
                LogicalTypeId::Integer => write!(out, "{}", column.data::<i32>()[row]).unwrap(),
                LogicalTypeId::BigInt => write!(out, "{}", column.data::<i64>()[row]).unwrap(),
                LogicalTypeId::Decimal => {
                    let v = column.data::<i64>()[row];
                    let sign = if v < 0 { "-" } else { "" };
                    write!(out, "{sign}{}.{:02}", v.unsigned_abs() / 100, v.unsigned_abs() % 100).unwrap();
                }
                LogicalTypeId::Date => {
                    let (y, m, d) = column.data::<Date>()[row].to_ymd();
                    write!(out, "{y:04}-{m:02}-{d:02}").unwrap();
                }
                LogicalTypeId::Varchar => out.extend_from_slice(column.str_at(row).expect("no NULLs")),
                other => panic!("unexpected column type {other:?}"),
            }
        }
        out.push(b'\n');
    }
}
