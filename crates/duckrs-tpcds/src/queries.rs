// Ported from duckdb/extension/tpcds/dsdgen/dsdgen.cpp (GetQuery, GetAnswer) and
// duckdb/extension/tpcds/tpcds_extension.cpp @ rust-port-baseline

//! The 99 TPC-DS queries and their answers, as DuckDB embeds them (`tpcds_constants.hpp`, made
//! from `dsdgen/queries/*.sql` and `dsdgen/answers/sf*/*.csv`).

use duckrs_common::{Error, ErrorKind, Result};

/// `TPCDS_QUERIES_COUNT`
pub const QUERIES_COUNT: u32 = 99;

/// The largest scale factor dsdgen supports (`DSDGEN_MAX_SCALE_FACTOR`).
pub(crate) const MAX_SCALE_FACTOR: f64 = 100000.0;

/// The scale factors C++'s `tpcds_answers()` lists answers for. C++ only embeds answers for 0.01
/// and 1, so its `tpcds_answers()` fails on scale factor 10; [`answer`] does the same.
pub const TPCDS_ANSWERS_SCALE_FACTORS: [f64; 2] = [1.0, 10.0];

static TPCDS_QUERIES: [&str; 99] = [
    include_str!("../data/queries/01.sql"),
    include_str!("../data/queries/02.sql"),
    include_str!("../data/queries/03.sql"),
    include_str!("../data/queries/04.sql"),
    include_str!("../data/queries/05.sql"),
    include_str!("../data/queries/06.sql"),
    include_str!("../data/queries/07.sql"),
    include_str!("../data/queries/08.sql"),
    include_str!("../data/queries/09.sql"),
    include_str!("../data/queries/10.sql"),
    include_str!("../data/queries/11.sql"),
    include_str!("../data/queries/12.sql"),
    include_str!("../data/queries/13.sql"),
    include_str!("../data/queries/14.sql"),
    include_str!("../data/queries/15.sql"),
    include_str!("../data/queries/16.sql"),
    include_str!("../data/queries/17.sql"),
    include_str!("../data/queries/18.sql"),
    include_str!("../data/queries/19.sql"),
    include_str!("../data/queries/20.sql"),
    include_str!("../data/queries/21.sql"),
    include_str!("../data/queries/22.sql"),
    include_str!("../data/queries/23.sql"),
    include_str!("../data/queries/24.sql"),
    include_str!("../data/queries/25.sql"),
    include_str!("../data/queries/26.sql"),
    include_str!("../data/queries/27.sql"),
    include_str!("../data/queries/28.sql"),
    include_str!("../data/queries/29.sql"),
    include_str!("../data/queries/30.sql"),
    include_str!("../data/queries/31.sql"),
    include_str!("../data/queries/32.sql"),
    include_str!("../data/queries/33.sql"),
    include_str!("../data/queries/34.sql"),
    include_str!("../data/queries/35.sql"),
    include_str!("../data/queries/36.sql"),
    include_str!("../data/queries/37.sql"),
    include_str!("../data/queries/38.sql"),
    include_str!("../data/queries/39.sql"),
    include_str!("../data/queries/40.sql"),
    include_str!("../data/queries/41.sql"),
    include_str!("../data/queries/42.sql"),
    include_str!("../data/queries/43.sql"),
    include_str!("../data/queries/44.sql"),
    include_str!("../data/queries/45.sql"),
    include_str!("../data/queries/46.sql"),
    include_str!("../data/queries/47.sql"),
    include_str!("../data/queries/48.sql"),
    include_str!("../data/queries/49.sql"),
    include_str!("../data/queries/50.sql"),
    include_str!("../data/queries/51.sql"),
    include_str!("../data/queries/52.sql"),
    include_str!("../data/queries/53.sql"),
    include_str!("../data/queries/54.sql"),
    include_str!("../data/queries/55.sql"),
    include_str!("../data/queries/56.sql"),
    include_str!("../data/queries/57.sql"),
    include_str!("../data/queries/58.sql"),
    include_str!("../data/queries/59.sql"),
    include_str!("../data/queries/60.sql"),
    include_str!("../data/queries/61.sql"),
    include_str!("../data/queries/62.sql"),
    include_str!("../data/queries/63.sql"),
    include_str!("../data/queries/64.sql"),
    include_str!("../data/queries/65.sql"),
    include_str!("../data/queries/66.sql"),
    include_str!("../data/queries/67.sql"),
    include_str!("../data/queries/68.sql"),
    include_str!("../data/queries/69.sql"),
    include_str!("../data/queries/70.sql"),
    include_str!("../data/queries/71.sql"),
    include_str!("../data/queries/72.sql"),
    include_str!("../data/queries/73.sql"),
    include_str!("../data/queries/74.sql"),
    include_str!("../data/queries/75.sql"),
    include_str!("../data/queries/76.sql"),
    include_str!("../data/queries/77.sql"),
    include_str!("../data/queries/78.sql"),
    include_str!("../data/queries/79.sql"),
    include_str!("../data/queries/80.sql"),
    include_str!("../data/queries/81.sql"),
    include_str!("../data/queries/82.sql"),
    include_str!("../data/queries/83.sql"),
    include_str!("../data/queries/84.sql"),
    include_str!("../data/queries/85.sql"),
    include_str!("../data/queries/86.sql"),
    include_str!("../data/queries/87.sql"),
    include_str!("../data/queries/88.sql"),
    include_str!("../data/queries/89.sql"),
    include_str!("../data/queries/90.sql"),
    include_str!("../data/queries/91.sql"),
    include_str!("../data/queries/92.sql"),
    include_str!("../data/queries/93.sql"),
    include_str!("../data/queries/94.sql"),
    include_str!("../data/queries/95.sql"),
    include_str!("../data/queries/96.sql"),
    include_str!("../data/queries/97.sql"),
    include_str!("../data/queries/98.sql"),
    include_str!("../data/queries/99.sql"),
];

static TPCDS_ANSWERS_SF0_01: [&str; 99] = [
    include_str!("../data/answers/sf0.01/01.csv"),
    include_str!("../data/answers/sf0.01/02.csv"),
    include_str!("../data/answers/sf0.01/03.csv"),
    include_str!("../data/answers/sf0.01/04.csv"),
    include_str!("../data/answers/sf0.01/05.csv"),
    include_str!("../data/answers/sf0.01/06.csv"),
    include_str!("../data/answers/sf0.01/07.csv"),
    include_str!("../data/answers/sf0.01/08.csv"),
    include_str!("../data/answers/sf0.01/09.csv"),
    include_str!("../data/answers/sf0.01/10.csv"),
    include_str!("../data/answers/sf0.01/11.csv"),
    include_str!("../data/answers/sf0.01/12.csv"),
    include_str!("../data/answers/sf0.01/13.csv"),
    include_str!("../data/answers/sf0.01/14.csv"),
    include_str!("../data/answers/sf0.01/15.csv"),
    include_str!("../data/answers/sf0.01/16.csv"),
    include_str!("../data/answers/sf0.01/17.csv"),
    include_str!("../data/answers/sf0.01/18.csv"),
    include_str!("../data/answers/sf0.01/19.csv"),
    include_str!("../data/answers/sf0.01/20.csv"),
    include_str!("../data/answers/sf0.01/21.csv"),
    include_str!("../data/answers/sf0.01/22.csv"),
    include_str!("../data/answers/sf0.01/23.csv"),
    include_str!("../data/answers/sf0.01/24.csv"),
    include_str!("../data/answers/sf0.01/25.csv"),
    include_str!("../data/answers/sf0.01/26.csv"),
    include_str!("../data/answers/sf0.01/27.csv"),
    include_str!("../data/answers/sf0.01/28.csv"),
    include_str!("../data/answers/sf0.01/29.csv"),
    include_str!("../data/answers/sf0.01/30.csv"),
    include_str!("../data/answers/sf0.01/31.csv"),
    include_str!("../data/answers/sf0.01/32.csv"),
    include_str!("../data/answers/sf0.01/33.csv"),
    include_str!("../data/answers/sf0.01/34.csv"),
    include_str!("../data/answers/sf0.01/35.csv"),
    include_str!("../data/answers/sf0.01/36.csv"),
    include_str!("../data/answers/sf0.01/37.csv"),
    include_str!("../data/answers/sf0.01/38.csv"),
    include_str!("../data/answers/sf0.01/39.csv"),
    include_str!("../data/answers/sf0.01/40.csv"),
    include_str!("../data/answers/sf0.01/41.csv"),
    include_str!("../data/answers/sf0.01/42.csv"),
    include_str!("../data/answers/sf0.01/43.csv"),
    include_str!("../data/answers/sf0.01/44.csv"),
    include_str!("../data/answers/sf0.01/45.csv"),
    include_str!("../data/answers/sf0.01/46.csv"),
    include_str!("../data/answers/sf0.01/47.csv"),
    include_str!("../data/answers/sf0.01/48.csv"),
    include_str!("../data/answers/sf0.01/49.csv"),
    include_str!("../data/answers/sf0.01/50.csv"),
    include_str!("../data/answers/sf0.01/51.csv"),
    include_str!("../data/answers/sf0.01/52.csv"),
    include_str!("../data/answers/sf0.01/53.csv"),
    include_str!("../data/answers/sf0.01/54.csv"),
    include_str!("../data/answers/sf0.01/55.csv"),
    include_str!("../data/answers/sf0.01/56.csv"),
    include_str!("../data/answers/sf0.01/57.csv"),
    include_str!("../data/answers/sf0.01/58.csv"),
    include_str!("../data/answers/sf0.01/59.csv"),
    include_str!("../data/answers/sf0.01/60.csv"),
    include_str!("../data/answers/sf0.01/61.csv"),
    include_str!("../data/answers/sf0.01/62.csv"),
    include_str!("../data/answers/sf0.01/63.csv"),
    include_str!("../data/answers/sf0.01/64.csv"),
    include_str!("../data/answers/sf0.01/65.csv"),
    include_str!("../data/answers/sf0.01/66.csv"),
    include_str!("../data/answers/sf0.01/67.csv"),
    include_str!("../data/answers/sf0.01/68.csv"),
    include_str!("../data/answers/sf0.01/69.csv"),
    include_str!("../data/answers/sf0.01/70.csv"),
    include_str!("../data/answers/sf0.01/71.csv"),
    include_str!("../data/answers/sf0.01/72.csv"),
    include_str!("../data/answers/sf0.01/73.csv"),
    include_str!("../data/answers/sf0.01/74.csv"),
    include_str!("../data/answers/sf0.01/75.csv"),
    include_str!("../data/answers/sf0.01/76.csv"),
    include_str!("../data/answers/sf0.01/77.csv"),
    include_str!("../data/answers/sf0.01/78.csv"),
    include_str!("../data/answers/sf0.01/79.csv"),
    include_str!("../data/answers/sf0.01/80.csv"),
    include_str!("../data/answers/sf0.01/81.csv"),
    include_str!("../data/answers/sf0.01/82.csv"),
    include_str!("../data/answers/sf0.01/83.csv"),
    include_str!("../data/answers/sf0.01/84.csv"),
    include_str!("../data/answers/sf0.01/85.csv"),
    include_str!("../data/answers/sf0.01/86.csv"),
    include_str!("../data/answers/sf0.01/87.csv"),
    include_str!("../data/answers/sf0.01/88.csv"),
    include_str!("../data/answers/sf0.01/89.csv"),
    include_str!("../data/answers/sf0.01/90.csv"),
    include_str!("../data/answers/sf0.01/91.csv"),
    include_str!("../data/answers/sf0.01/92.csv"),
    include_str!("../data/answers/sf0.01/93.csv"),
    include_str!("../data/answers/sf0.01/94.csv"),
    include_str!("../data/answers/sf0.01/95.csv"),
    include_str!("../data/answers/sf0.01/96.csv"),
    include_str!("../data/answers/sf0.01/97.csv"),
    include_str!("../data/answers/sf0.01/98.csv"),
    include_str!("../data/answers/sf0.01/99.csv"),
];

static TPCDS_ANSWERS_SF1: [&str; 99] = [
    include_str!("../data/answers/sf1/01.csv"),
    include_str!("../data/answers/sf1/02.csv"),
    include_str!("../data/answers/sf1/03.csv"),
    include_str!("../data/answers/sf1/04.csv"),
    include_str!("../data/answers/sf1/05.csv"),
    include_str!("../data/answers/sf1/06.csv"),
    include_str!("../data/answers/sf1/07.csv"),
    include_str!("../data/answers/sf1/08.csv"),
    include_str!("../data/answers/sf1/09.csv"),
    include_str!("../data/answers/sf1/10.csv"),
    include_str!("../data/answers/sf1/11.csv"),
    include_str!("../data/answers/sf1/12.csv"),
    include_str!("../data/answers/sf1/13.csv"),
    include_str!("../data/answers/sf1/14.csv"),
    include_str!("../data/answers/sf1/15.csv"),
    include_str!("../data/answers/sf1/16.csv"),
    include_str!("../data/answers/sf1/17.csv"),
    include_str!("../data/answers/sf1/18.csv"),
    include_str!("../data/answers/sf1/19.csv"),
    include_str!("../data/answers/sf1/20.csv"),
    include_str!("../data/answers/sf1/21.csv"),
    include_str!("../data/answers/sf1/22.csv"),
    include_str!("../data/answers/sf1/23.csv"),
    include_str!("../data/answers/sf1/24.csv"),
    include_str!("../data/answers/sf1/25.csv"),
    include_str!("../data/answers/sf1/26.csv"),
    include_str!("../data/answers/sf1/27.csv"),
    include_str!("../data/answers/sf1/28.csv"),
    include_str!("../data/answers/sf1/29.csv"),
    include_str!("../data/answers/sf1/30.csv"),
    include_str!("../data/answers/sf1/31.csv"),
    include_str!("../data/answers/sf1/32.csv"),
    include_str!("../data/answers/sf1/33.csv"),
    include_str!("../data/answers/sf1/34.csv"),
    include_str!("../data/answers/sf1/35.csv"),
    include_str!("../data/answers/sf1/36.csv"),
    include_str!("../data/answers/sf1/37.csv"),
    include_str!("../data/answers/sf1/38.csv"),
    include_str!("../data/answers/sf1/39.csv"),
    include_str!("../data/answers/sf1/40.csv"),
    include_str!("../data/answers/sf1/41.csv"),
    include_str!("../data/answers/sf1/42.csv"),
    include_str!("../data/answers/sf1/43.csv"),
    include_str!("../data/answers/sf1/44.csv"),
    include_str!("../data/answers/sf1/45.csv"),
    include_str!("../data/answers/sf1/46.csv"),
    include_str!("../data/answers/sf1/47.csv"),
    include_str!("../data/answers/sf1/48.csv"),
    include_str!("../data/answers/sf1/49.csv"),
    include_str!("../data/answers/sf1/50.csv"),
    include_str!("../data/answers/sf1/51.csv"),
    include_str!("../data/answers/sf1/52.csv"),
    include_str!("../data/answers/sf1/53.csv"),
    include_str!("../data/answers/sf1/54.csv"),
    include_str!("../data/answers/sf1/55.csv"),
    include_str!("../data/answers/sf1/56.csv"),
    include_str!("../data/answers/sf1/57.csv"),
    include_str!("../data/answers/sf1/58.csv"),
    include_str!("../data/answers/sf1/59.csv"),
    include_str!("../data/answers/sf1/60.csv"),
    include_str!("../data/answers/sf1/61.csv"),
    include_str!("../data/answers/sf1/62.csv"),
    include_str!("../data/answers/sf1/63.csv"),
    include_str!("../data/answers/sf1/64.csv"),
    include_str!("../data/answers/sf1/65.csv"),
    include_str!("../data/answers/sf1/66.csv"),
    include_str!("../data/answers/sf1/67.csv"),
    include_str!("../data/answers/sf1/68.csv"),
    include_str!("../data/answers/sf1/69.csv"),
    include_str!("../data/answers/sf1/70.csv"),
    include_str!("../data/answers/sf1/71.csv"),
    include_str!("../data/answers/sf1/72.csv"),
    include_str!("../data/answers/sf1/73.csv"),
    include_str!("../data/answers/sf1/74.csv"),
    include_str!("../data/answers/sf1/75.csv"),
    include_str!("../data/answers/sf1/76.csv"),
    include_str!("../data/answers/sf1/77.csv"),
    include_str!("../data/answers/sf1/78.csv"),
    include_str!("../data/answers/sf1/79.csv"),
    include_str!("../data/answers/sf1/80.csv"),
    include_str!("../data/answers/sf1/81.csv"),
    include_str!("../data/answers/sf1/82.csv"),
    include_str!("../data/answers/sf1/83.csv"),
    include_str!("../data/answers/sf1/84.csv"),
    include_str!("../data/answers/sf1/85.csv"),
    include_str!("../data/answers/sf1/86.csv"),
    include_str!("../data/answers/sf1/87.csv"),
    include_str!("../data/answers/sf1/88.csv"),
    include_str!("../data/answers/sf1/89.csv"),
    include_str!("../data/answers/sf1/90.csv"),
    include_str!("../data/answers/sf1/91.csv"),
    include_str!("../data/answers/sf1/92.csv"),
    include_str!("../data/answers/sf1/93.csv"),
    include_str!("../data/answers/sf1/94.csv"),
    include_str!("../data/answers/sf1/95.csv"),
    include_str!("../data/answers/sf1/96.csv"),
    include_str!("../data/answers/sf1/97.csv"),
    include_str!("../data/answers/sf1/98.csv"),
    include_str!("../data/answers/sf1/99.csv"),
];

fn check_query(query: i32) -> Result<usize> {
    if query <= 0 || query > QUERIES_COUNT as i32 {
        return Err(Error::new(
            ErrorKind::Syntax,
            format!("Out of range TPC-DS query number {query}"),
        ));
    }
    Ok((query - 1) as usize)
}

/// `DSDGenWrapper::GetQuery`: TPC-DS query `query` (1 to 99).
pub fn query(query: i32) -> Result<&'static str> {
    Ok(TPCDS_QUERIES[check_query(query)?])
}

/// `ScaleQ9Thresholds`: query 9 compares store_sales counts against thresholds that qgen draws
/// from a range proportional to the scale factor; this scales the stored SF 1 thresholds so the
/// `CASE` branches keep their selectivity.
fn scale_q9_thresholds(query: &str, sf: f64) -> String {
    let bytes = query.as_bytes();
    let mut result = String::with_capacity(query.len());
    let mut pos = 0;
    let mut scaled_count = 0;
    while pos < bytes.len() {
        let Some(found) = query[pos..].find(") > ") else {
            break;
        };
        let start = pos + found + 4;
        let mut end = start;
        let mut threshold: i64 = 0;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            threshold = threshold * 10 + i64::from(bytes[end] - b'0');
            end += 1;
        }
        result.push_str(&query[pos..start]);
        pos = end;
        if end == start || !query[end..].starts_with(" THEN") {
            result.push_str(&query[start..end]);
            continue;
        }
        let scaled = ((threshold as f64 * sf).round() as i64).max(1);
        result.push_str(&scaled.to_string());
        scaled_count += 1;
    }
    result.push_str(&query[pos..]);
    debug_assert_eq!(scaled_count, 5);
    result
}

/// `DSDGenWrapper::GetQuery(query, sf)`: query `query` with the substitution parameters for scale
/// factor `sf` (only query 9 has any).
pub fn query_for_scale(query_nr: i32, sf: f64) -> Result<String> {
    if sf.is_nan() || sf <= 0.0 || sf > MAX_SCALE_FACTOR {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "TPC-DS queries require a scale factor between 0 and {}",
                MAX_SCALE_FACTOR as i64
            ),
        ));
    }
    let result = query(query_nr)?;
    if query_nr != 9 {
        return Ok(result.to_string());
    }
    Ok(scale_q9_thresholds(result, sf))
}

/// `DSDGenWrapper::GetAnswer`: the answer of query `query` at scale factor `sf`, for scale
/// factors 0.01 and 1.
pub fn answer(sf: f64, query: i32) -> Result<&'static str> {
    let index = check_query(query)?;
    if sf == 0.01 {
        Ok(TPCDS_ANSWERS_SF0_01[index])
    } else if sf == 1.0 {
        Ok(TPCDS_ANSWERS_SF1[index])
    } else {
        Err(Error::not_implemented(format!(
            "Don't have TPC-DS answers for SF {sf:.6}!"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_and_answers() {
        assert!(query(1).unwrap().starts_with("WITH customer_total_return AS"));
        assert!(query(0).is_err());
        assert!(query(100).is_err());
        assert_eq!(query_for_scale(1, 0.01).unwrap(), query(1).unwrap());
        assert!(query_for_scale(1, 0.0).is_err());
        let q9 = query_for_scale(9, 0.01).unwrap();
        assert_ne!(q9, query(9).unwrap());
        assert_eq!(query_for_scale(9, 1.0).unwrap(), query(9).unwrap());
        assert!(!answer(0.01, 1).unwrap().is_empty());
        assert!(!answer(1.0, 99).unwrap().is_empty());
        assert!(answer(10.0, 1).is_err());
    }
}
