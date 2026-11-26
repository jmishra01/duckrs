//! Renders generated chunks as the pipe-separated text `tools/tpcds/driver.cpp` writes: one line
//! per row, columns separated by `|`, NULL as nothing.

use duckrs_common::types::Date;
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_tpcds::{ColumnType, Table};

/// The civil date of a day count from 1970-01-01, as `YYYY-MM-DD` (the driver's `civil_from_days`).
fn render_date(out: &mut Vec<u8>, days: i32) {
    let z = i64::from(days) + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    out.extend_from_slice(format!("{year:04}-{month:02}-{day:02}").as_bytes());
}

fn render_decimal(out: &mut Vec<u8>, number: i64, scale: u8) {
    let unit = 10i64.pow(u32::from(scale));
    let mut number = number;
    if number < 0 {
        out.push(b'-');
        number = -number;
    }
    out.extend_from_slice(format!("{}.{:0width$}", number / unit, number % unit, width = scale as usize).as_bytes());
}

fn render_value(out: &mut Vec<u8>, vector: &Vector, ty: ColumnType, row: usize) {
    if !vector.is_valid(row) {
        return;
    }
    match ty {
        ColumnType::Varchar => out.extend_from_slice(vector.str_at(row).expect("a valid string")),
        ColumnType::BigInt => out.extend_from_slice(vector.data::<i64>()[row].to_string().as_bytes()),
        ColumnType::Integer => out.extend_from_slice(vector.data::<i32>()[row].to_string().as_bytes()),
        ColumnType::Date => render_date(out, vector.data::<Date>()[row].0),
        ColumnType::Decimal(width, scale) => {
            let number = if width <= 4 {
                i64::from(vector.data::<i16>()[row])
            } else if width <= 9 {
                i64::from(vector.data::<i32>()[row])
            } else {
                vector.data::<i64>()[row]
            };
            render_decimal(out, number, scale);
        }
    }
}

/// Appends the rows of `chunk`, a chunk of `table`, to `out`.
pub fn render_chunk(out: &mut Vec<u8>, table: Table, chunk: &DataChunk) {
    let columns = table.info().columns;
    assert_eq!(chunk.column_count(), columns.len());
    for row in 0..chunk.len() {
        for (i, &(_, ty)) in columns.iter().enumerate() {
            if i > 0 {
                out.push(b'|');
            }
            render_value(out, chunk.column(i), ty, row);
        }
        out.push(b'\n');
    }
}

/// Generates all tables at scale factor `sf` and renders each.
pub fn render_all(sf: f64) -> Vec<Vec<u8>> {
    let mut tables = vec![Vec::new(); Table::ALL.len()];
    for (table, chunk) in duckrs_tpcds::dsdgen(sf).expect("a valid scale factor") {
        assert!(chunk.len() <= duckrs_common::STANDARD_VECTOR_SIZE);
        assert_eq!(chunk.types(), table.types());
        render_chunk(&mut tables[table as usize], table, &chunk);
    }
    tables
}
