use super::*;
use crate::comparison::ComparisonType;
use crate::types::LogicalType;
use crate::value::Value;
use crate::vector::{DataChunk, SelectionVector};
use proptest::prelude::*;

fn struct_type() -> LogicalType {
    LogicalType::struct_type(vec![
        ("k".into(), LogicalType::INTEGER),
        ("s".into(), LogicalType::VARCHAR),
    ])
}

fn sample_chunk() -> DataChunk {
    let types = [
        LogicalType::INTEGER,
        LogicalType::VARCHAR,
        LogicalType::DOUBLE,
        struct_type(),
    ];
    let mut chunk = DataChunk::with_capacity(&types, 4);
    let rows = [
        (
            Some(1),
            Some("short"),
            Some(1.5),
            Some((1, "a long string inside a struct")),
        ),
        (None, Some("a string long enough to live in the heap"), None, None),
        (Some(3), None, Some(f64::NAN), Some((3, "x"))),
    ];
    for (i, s, d, st) in rows {
        let row = [
            i.map_or(Value::null(LogicalType::INTEGER), Value::integer),
            s.map_or(Value::null(LogicalType::VARCHAR), Value::varchar),
            d.map_or(Value::null(LogicalType::DOUBLE), Value::double),
            st.map_or(Value::null(struct_type()), |(k, s)| {
                Value::struct_value(vec![("k".into(), Value::integer(k)), ("s".into(), Value::varchar(s))])
            }),
        ];
        chunk.append_row(&row).unwrap();
    }
    chunk
}

fn layout_for(chunk: &DataChunk) -> TupleDataLayout {
    TupleDataLayout::new(chunk.types(), vec![8], RowValidity::CanHaveNull)
}

#[test]
fn round_trips_rows() {
    let chunk = sample_chunk();
    let mut rows = TupleDataCollection::new(layout_for(&chunk)).unwrap();
    let ptrs = rows.append(&chunk);
    assert_eq!(ptrs.len(), 3);
    drop(chunk);
    let chunk = sample_chunk();
    let back: Vec<DataChunk> = rows.scan().collect();
    assert_eq!(back.len(), 1);
    for r in 0..3 {
        for c in 0..4 {
            let (a, b) = (back[0].get_value(c, r), chunk.get_value(c, r));
            let both_nan = matches!((a.data(), b.data()), (crate::value::ValueData::Double(x), crate::value::ValueData::Double(y)) if x.is_nan() && y.is_nan());
            assert!(a == b || both_nan, "row {r} column {c}: {a:?} vs {b:?}");
        }
    }
}

#[test]
fn gathered_strings_outlive_the_scan() {
    let chunk = sample_chunk();
    let mut rows = TupleDataCollection::new(layout_for(&chunk)).unwrap();
    let ptrs = rows.append(&chunk);
    let mut target = crate::vector::Vector::new(LogicalType::VARCHAR, 1);
    // SAFETY: ptrs came from `rows`, which is alive.
    unsafe { rows.gather(&ptrs[1..2], 1, &mut target) };
    drop(rows);
    assert_eq!(
        target.get_value(0),
        Value::varchar("a string long enough to live in the heap")
    );
}

#[test]
fn aggregate_states_are_aligned_and_zeroed() {
    let chunk = sample_chunk();
    let layout = layout_for(&chunk);
    let mut rows = TupleDataCollection::new(layout.clone()).unwrap();
    let ptrs = rows.append(&chunk);
    for p in &ptrs {
        // SAFETY: rows are alive; the state is 8 bytes at an 8-aligned offset in the row.
        unsafe {
            let state = p.aggregate_state(&layout, 0);
            assert_eq!(layout.aggregate_offset(0) % 8, 0);
            assert_eq!(state.cast::<u64>().read_unaligned(), 0);
            state.cast::<u64>().write_unaligned(42);
        }
    }
}

#[test]
fn matcher_semantics() {
    let chunk = sample_chunk();
    let layout = layout_for(&chunk);
    let mut rows = TupleDataCollection::new(layout.clone()).unwrap();
    let ptrs = rows.append(&chunk);
    let all = || vec![0u32, 1, 2];

    // Every row against itself: EQUAL rejects rows with any NULL key; NOT DISTINCT FROM accepts them.
    let equal = RowMatcher::new(&layout, vec![ComparisonType::Equal; 4]).unwrap();
    let not_distinct = RowMatcher::new(&layout, vec![ComparisonType::NotDistinctFrom; 4]).unwrap();
    let (mut sel, mut misses) = (all(), Vec::new());
    // SAFETY: ptrs belong to the live collection `rows`.
    unsafe { equal.match_rows(&rows, &chunk, &ptrs, &mut sel, Some(&mut misses)) };
    assert_eq!(sel, [0]);
    assert_eq!(misses, [1, 2]);
    let mut sel = all();
    // SAFETY: as above.
    unsafe { not_distinct.match_rows(&rows, &chunk, &ptrs, &mut sel, None) };
    assert_eq!(sel, [0, 1, 2], "NaN matches NaN, NULL matches NULL");

    // Row 0's key against every row.
    let shifted = vec![ptrs[0]; 3];
    let mut sel = all();
    // SAFETY: as above.
    unsafe { not_distinct.match_rows(&rows, &chunk, &shifted, &mut sel, None) };
    assert_eq!(sel, [0]);

    let gt = RowMatcher::new(&layout, vec![ComparisonType::GreaterThan]).unwrap();
    let mut sel = all();
    // SAFETY: as above.
    unsafe { gt.match_rows(&rows, &chunk, &shifted, &mut sel, None) };
    assert_eq!(sel, [2], "3 > 1; NULL > 1 is not true");
    assert!(
        RowMatcher::new(
            &layout,
            vec![
                ComparisonType::Equal,
                ComparisonType::Equal,
                ComparisonType::Equal,
                ComparisonType::LessThan
            ]
        )
        .is_err()
    );
}

fn int_list(v: &[Option<i32>]) -> Value {
    Value::list(
        LogicalType::INTEGER,
        v.iter()
            .map(|x| x.map_or(Value::null(LogicalType::INTEGER), Value::integer))
            .collect(),
    )
    .unwrap()
}

#[test]
fn lists_and_arrays_round_trip_and_match() {
    let list_ty = LogicalType::list(LogicalType::INTEGER);
    let arr_ty = LogicalType::array(LogicalType::VARCHAR, 2);
    let nested_ty = LogicalType::struct_type(vec![("l".into(), LogicalType::list(LogicalType::VARCHAR))]);
    let types = [list_ty.clone(), arr_ty.clone(), nested_ty.clone()];
    let arr = |a: &str, b: &str| {
        Value::new(
            arr_ty.clone(),
            crate::value::ValueData::List(vec![Value::varchar(a), Value::varchar(b)]),
        )
        .unwrap()
    };
    let nested = |v: &[&str]| {
        Value::struct_value(vec![(
            "l".into(),
            Value::list(LogicalType::VARCHAR, v.iter().map(|s| Value::varchar(*s)).collect()).unwrap(),
        )])
    };
    let rows_in = [
        [
            int_list(&[Some(1), None]),
            arr("x", "a string long enough for the heap"),
            nested(&["p", "q"]),
        ],
        [
            Value::null(list_ty.clone()),
            Value::null(arr_ty.clone()),
            Value::null(nested_ty.clone()),
        ],
        [int_list(&[]), arr("", "y"), nested(&[])],
    ];
    let mut chunk = DataChunk::with_capacity(&types, 3);
    for row in &rows_in {
        chunk.append_row(row).unwrap();
    }
    let layout = TupleDataLayout::new(types.to_vec(), vec![], RowValidity::CanHaveNull);
    let mut rows = TupleDataCollection::new(layout.clone()).unwrap();
    let ptrs = rows.append(&chunk);
    let ptrs2 = rows.append(&chunk);
    drop(chunk);
    let back: Vec<DataChunk> = rows.scan().collect();
    for (r, row) in rows_in.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            assert_eq!(&back[0].get_value(c, r), v, "row {r} column {c}");
            assert_eq!(&back[0].get_value(c, r + 3), v, "second copy, row {r} column {c}");
        }
    }

    // Each row against its copy: NOT DISTINCT FROM matches all; EQUAL rejects the NULL row.
    let mut chunk = DataChunk::with_capacity(&types, 3);
    for row in &rows_in {
        chunk.append_row(row).unwrap();
    }
    let not_distinct = RowMatcher::new(&layout, vec![ComparisonType::NotDistinctFrom; 3]).unwrap();
    let equal = RowMatcher::new(&layout, vec![ComparisonType::Equal; 2]).unwrap();
    let mut sel = vec![0, 1, 2];
    // SAFETY: ptrs2 belong to `rows`, which is alive.
    unsafe { not_distinct.match_rows(&rows, &chunk, &ptrs2, &mut sel, None) };
    assert_eq!(sel, [0, 1, 2]);
    let mut sel = vec![0, 1, 2];
    // SAFETY: as above.
    unsafe { equal.match_rows(&rows, &chunk, &ptrs, &mut sel, None) };
    assert_eq!(sel, [0, 2], "[1, NULL] = [1, NULL] is true; NULL = NULL is not");

    // Ordering: every row against row 0's list [1, NULL]. [] < [1, NULL]; NULL never compares.
    let lt = RowMatcher::new(&layout, vec![ComparisonType::LessThan]).unwrap();
    let against_first = vec![ptrs[0]; 3];
    let mut sel = vec![0, 1, 2];
    // SAFETY: as above.
    unsafe { lt.match_rows(&rows, &chunk, &against_first, &mut sel, None) };
    assert_eq!(sel, [2]);
}

proptest! {
    /// Matching lists through sort keys agrees with DuckDB's list order (`[1, NULL] > [1, 2]`,
    /// shorter lists first), which is the order of Vec<Option<i32>> with None greatest.
    #[test]
    fn list_comparisons_follow_duckdb_order(
        a in prop::collection::vec(prop::option::of(-3i32..3), 0..4),
        b in prop::collection::vec(prop::option::of(-3i32..3), 0..4),
    ) {
        let types = [LogicalType::list(LogicalType::INTEGER)];
        let layout = TupleDataLayout::new(types.to_vec(), vec![], RowValidity::CanHaveNull);
        let mut rows = TupleDataCollection::new(layout.clone()).unwrap();
        let mut stored = DataChunk::with_capacity(&types, 1);
        stored.append_row(&[int_list(&b)]).unwrap();
        let ptrs = rows.append(&stored);
        let mut probe = DataChunk::with_capacity(&types, 1);
        probe.append_row(&[int_list(&a)]).unwrap();
        // None sorts after Some, like a NULL element.
        let key = |v: &Vec<Option<i32>>| v.iter().map(|x| x.map_or((1, 0), |x| (0, x))).collect::<Vec<_>>();
        let expected = key(&a).cmp(&key(&b));
        for (predicate, holds) in [
            (ComparisonType::Equal, expected.is_eq()),
            (ComparisonType::LessThan, expected.is_lt()),
            (ComparisonType::GreaterThanOrEqual, expected.is_ge()),
        ] {
            let matcher = RowMatcher::new(&layout, vec![predicate]).unwrap();
            let mut sel = vec![0];
            // SAFETY: ptrs belong to `rows`, which is alive.
            unsafe { matcher.match_rows(&rows, &probe, &ptrs, &mut sel, None) };
            prop_assert_eq!(!sel.is_empty(), holds, "{:?} {:?} {:?}", predicate, a, b);
        }
    }
}

#[test]
fn many_rows_span_blocks() {
    let types = [LogicalType::BIGINT, LogicalType::VARCHAR];
    let layout = TupleDataLayout::new(types.to_vec(), vec![], RowValidity::CanHaveNull);
    let mut rows = TupleDataCollection::new(layout).unwrap();
    for b in 0..20 {
        let mut chunk = DataChunk::with_capacity(&types, 2048);
        for i in 0..2048i64 {
            let v = b * 2048 + i;
            chunk
                .append_row(&[
                    Value::bigint(v),
                    Value::varchar(format!("value number {v} with padding")),
                ])
                .unwrap();
        }
        rows.append(&chunk);
    }
    assert_eq!(rows.len(), 40_960);
    let mut seen = 0i64;
    for chunk in rows.scan() {
        for r in 0..chunk.len() {
            assert_eq!(chunk.get_value(0, r), Value::bigint(seen));
            seen += 1;
        }
    }
    assert_eq!(seen, 40_960);
    let mut sliced = rows.scan().next().unwrap();
    sliced.slice(&SelectionVector::from_vec(vec![5]), 1);
    assert_eq!(sliced.get_value(1, 0), Value::varchar("value number 5 with padding"));
    rows.reset();
    assert!(rows.is_empty());
}

proptest! {
    #[test]
    fn random_rows_round_trip(values in prop::collection::vec((prop::option::of(any::<i64>()), prop::option::of("[a-z]{0,30}")), 1..500)) {
        let types = [LogicalType::BIGINT, LogicalType::VARCHAR];
        let mut chunk = DataChunk::with_capacity(&types, values.len());
        for (i, s) in &values {
            chunk.append_row(&[
                i.map_or(Value::null(LogicalType::BIGINT), Value::bigint),
                s.as_ref().map_or(Value::null(LogicalType::VARCHAR), |s| Value::varchar(s.clone())),
            ]).unwrap();
        }
        let layout = TupleDataLayout::new(types.to_vec(), vec![], RowValidity::CanHaveNull);
        let mut rows = TupleDataCollection::new(layout).unwrap();
        rows.append(&chunk);
        let back = rows.scan().next().unwrap();
        for r in 0..values.len() {
            prop_assert_eq!(back.get_value(0, r), chunk.get_value(0, r));
            prop_assert_eq!(back.get_value(1, r), chunk.get_value(1, r));
        }
    }
}
