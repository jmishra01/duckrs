use super::*;
use duckrs_common::STANDARD_VECTOR_SIZE;
use duckrs_common::sort_key::{RowKey, RowKeyMap, create_row_keys};
use duckrs_common::types::{Interval, Time, TimeTz};
use duckrs_common::value::Value;
use duckrs_common::vector::Vector;
use proptest::prelude::*;

/// Per column of the schema, its type and the values a row picks from: few, so that rows repeat, and with each
/// pair that groups must take as equal (-0.0 and 0.0, NaNs, '1 month' and '30 days', a NULL struct, ...).
fn schema() -> Vec<(LogicalType, Vec<Value>)> {
    let interval = |months, days, micros| Value::interval(Interval { months, days, micros });
    let time_tz = |micros, offset| Value::time_tz(TimeTz::new(Time(micros), offset));
    let list = |items: Vec<Value>| Value::list(LogicalType::INTEGER, items).unwrap();
    let point = LogicalType::struct_type(vec![
        ("a".into(), LogicalType::INTEGER),
        ("b".into(), LogicalType::VARCHAR),
    ]);
    let field = |a: Value, b: Value| Value::struct_value(vec![("a".into(), a), ("b".into(), b)]);
    let decimal = LogicalType::decimal(38, 2);
    let hour = 3_600_000_000;
    vec![
        (
            LogicalType::INTEGER,
            vec![
                Value::null(LogicalType::INTEGER),
                Value::integer(0),
                Value::integer(1),
                Value::integer(-7),
            ],
        ),
        (
            LogicalType::DOUBLE,
            vec![
                Value::null(LogicalType::DOUBLE),
                Value::double(0.0),
                Value::double(-0.0),
                Value::double(f64::NAN),
                Value::double(-f64::NAN),
                Value::double(1.5),
            ],
        ),
        (
            LogicalType::VARCHAR,
            vec![
                Value::null(LogicalType::VARCHAR),
                Value::varchar(""),
                Value::varchar("a"),
                Value::varchar("A"),
                Value::varchar("a string longer than twelve bytes"),
                Value::varchar("a string longer than twelve bytes!"),
            ],
        ),
        (
            LogicalType::INTERVAL,
            vec![
                Value::null(LogicalType::INTERVAL),
                interval(1, 0, 0),
                interval(0, 30, 0),
                interval(0, 1, 0),
                interval(0, 0, 24 * hour),
            ],
        ),
        (
            LogicalType::list(LogicalType::INTEGER),
            vec![
                Value::null(LogicalType::list(LogicalType::INTEGER)),
                list(vec![]),
                list(vec![Value::integer(1)]),
                list(vec![Value::integer(1), Value::null(LogicalType::INTEGER)]),
                list(vec![Value::null(LogicalType::INTEGER)]),
            ],
        ),
        (
            point.clone(),
            vec![
                Value::null(point),
                field(Value::integer(1), Value::varchar("x")),
                field(Value::null(LogicalType::INTEGER), Value::varchar("x")),
                field(Value::integer(1), Value::null(LogicalType::VARCHAR)),
            ],
        ),
        (
            decimal.clone(),
            vec![
                Value::null(decimal),
                Value::decimal(0, 38, 2).unwrap(),
                Value::decimal(100, 38, 2).unwrap(),
                Value::decimal(-100, 38, 2).unwrap(),
            ],
        ),
        (
            Value::time_tz(TimeTz::new(Time(0), 0)).logical_type().clone(),
            vec![
                time_tz(12 * hour, 0),
                time_tz(13 * hour, 3600),
                time_tz(12 * hour, 3600),
                Value::null(Value::time_tz(TimeTz::new(Time(0), 0)).logical_type().clone()),
            ],
        ),
    ]
}

/// The chunks of `rows`, of the columns `columns` of the schema, each row the value it picks of each.
fn chunks(columns: &[usize], rows: &[Vec<usize>]) -> Vec<DataChunk> {
    let schema = schema();
    rows.chunks(STANDARD_VECTOR_SIZE)
        .map(|part| {
            let vectors = columns
                .iter()
                .map(|&column| {
                    let (ty, values) = &schema[column];
                    let mut vector = Vector::new(ty.clone(), part.len());
                    vector.prepare_flat(part.len());
                    for (row, picks) in part.iter().enumerate() {
                        vector.set_value(row, &values[picks[column] % values.len()]).unwrap();
                    }
                    vector
                })
                .collect();
            DataChunk::from_columns(vectors, part.len())
        })
        .collect()
}

/// The group of each row by `GroupTable`, and by the keys of `create_row_keys`; and the table.
fn grouped(chunks: &[DataChunk]) -> (Vec<usize>, Vec<usize>, GroupTable) {
    let mut table = GroupTable::new(chunks[0].types().to_vec()).unwrap();
    let mut keys: RowKeyMap<usize> = RowKeyMap::default();
    let (mut by_table, mut by_keys) = (Vec::new(), Vec::new());
    for chunk in chunks {
        let hashes = GroupTable::hash(chunk).unwrap();
        let (ids, _) = table.find_or_create(chunk, &hashes).unwrap();
        by_table.extend(ids);
        let columns: Vec<&Vector> = chunk.columns().iter().collect();
        for key in create_row_keys(&columns, chunk.len()).unwrap() {
            let next = keys.len();
            by_keys.push(*keys.entry(key).or_insert(next));
        }
    }
    (by_table, by_keys, table)
}

/// Both put the rows into the same groups: a group of one is a group of the other, and no more.
fn same_groups(a: &[usize], b: &[usize]) {
    let mut a_to_b = std::collections::HashMap::new();
    let mut b_to_a = std::collections::HashMap::new();
    for (row, (&x, &y)) in a.iter().zip(b).enumerate() {
        assert_eq!(
            *a_to_b.entry(x).or_insert(y),
            y,
            "row {row}: one group of the table is two of the keys"
        );
        assert_eq!(
            *b_to_a.entry(y).or_insert(x),
            x,
            "row {row}: one group of the keys is two of the table"
        );
    }
}

/// The key of each row of `chunks`, in order.
fn row_keys(chunks: &[DataChunk]) -> Vec<RowKey> {
    chunks
        .iter()
        .flat_map(|chunk| {
            let columns: Vec<&Vector> = chunk.columns().iter().collect();
            create_row_keys(&columns, chunk.len()).unwrap()
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Groups of each column alone and of all of them are those of the keys, and the values of each group are
    /// those of the first row of it.
    #[test]
    fn groups_are_those_of_the_keys(rows in proptest::collection::vec(proptest::collection::vec(0usize..16, 8), 1..5000)) {
        let all: Vec<usize> = (0..schema().len()).collect();
        let mut sets: Vec<Vec<usize>> = all.iter().map(|&column| vec![column]).collect();
        sets.push(all);
        for columns in sets {
            let input = chunks(&columns, &rows);
            let (by_table, by_keys, table) = grouped(&input);
            same_groups(&by_table, &by_keys);
            let stored: Vec<DataChunk> = table.scan().collect();
            let stored_keys = row_keys(&stored);
            prop_assert_eq!(stored_keys.len(), table.len());
            let input_keys = row_keys(&input);
            let mut first = vec![usize::MAX; table.len()];
            for (row, &id) in by_table.iter().enumerate() {
                if first[id] == usize::MAX {
                    first[id] = row;
                }
            }
            for (id, &row) in first.iter().enumerate() {
                prop_assert_eq!(&stored_keys[id], &input_keys[row], "group {} of columns {:?}", id, &columns);
            }
        }
    }
}

/// Each group is found again by `find`, which makes none; rows of no group find none.
#[test]
fn find_makes_no_group() {
    let input = chunks(
        &[0, 2],
        &(0..300)
            .map(|i| vec![i % 4, i % 5, i % 6, 0, 0, 0, 0, 0])
            .collect::<Vec<_>>(),
    );
    let (by_table, _, table) = grouped(&input);
    let hashes = GroupTable::hash(&input[0]).unwrap();
    let found = table.find(&input[0], &hashes);
    assert!(found.iter().zip(&by_table).all(|(found, &id)| *found == Some(id)));
    // `i % 4` and `i % 6` are both odd or both even: 12 pairs
    assert_eq!(table.len(), 12);
    let other = chunks(&[0, 2], &[vec![3, 0, 4, 0, 0, 0, 0, 0], vec![2, 0, 4, 0, 0, 0, 0, 0]]);
    let hashes = GroupTable::hash(&other[0]).unwrap();
    // an odd pick of the integer with an even one of the string is no group, two even ones are
    assert_eq!(
        table
            .find(&other[0], &hashes)
            .iter()
            .map(Option::is_some)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(table.len(), 12);
}

/// Groups past several sizes of the table: each distinct, then each again, are each one group.
#[test]
fn groups_survive_resizing() {
    let distinct = 100_000usize;
    let mut table = GroupTable::new(vec![LogicalType::BIGINT, LogicalType::VARCHAR]).unwrap();
    for round in 0..2 {
        for start in (0..distinct).step_by(STANDARD_VECTOR_SIZE) {
            let count = STANDARD_VECTOR_SIZE.min(distinct - start);
            let mut numbers = Vector::new(LogicalType::BIGINT, count);
            let mut strings = Vector::new(LogicalType::VARCHAR, count);
            numbers.prepare_flat(count);
            strings.prepare_flat(count);
            for row in 0..count {
                let i = (start + row) as i64;
                numbers.set_value(row, &Value::bigint(i / 3)).unwrap();
                strings
                    .set_value(row, &Value::varchar(format!("group number {}", i % 3)))
                    .unwrap();
            }
            let chunk = DataChunk::from_columns(vec![numbers, strings], count);
            let hashes = GroupTable::hash(&chunk).unwrap();
            let (ids, new_rows) = table.find_or_create(&chunk, &hashes).unwrap();
            for (row, &id) in ids.iter().enumerate() {
                assert_eq!(id, start + row, "round {round}");
            }
            assert_eq!(new_rows.len(), if round == 0 { count } else { 0 });
        }
    }
    assert_eq!(table.len(), distinct);
}

/// What can't be grouped as the keys group isn't taken.
#[test]
fn variant_and_bit_keep_the_keys() {
    assert!(GroupTable::supports(&[
        LogicalType::INTEGER,
        LogicalType::list(LogicalType::VARCHAR)
    ]));
    assert!(!GroupTable::supports(&[]));
    assert!(!GroupTable::supports(&[LogicalType::new(LogicalTypeId::Bit)]));
    let nested = LogicalType::struct_type(vec![("v".into(), LogicalType::new(LogicalTypeId::Variant))]);
    assert!(!GroupTable::supports(&[LogicalType::INTEGER, nested]));
    assert!(!GroupTable::supports(&[LogicalType::list(LogicalType::new(
        LogicalTypeId::Bit
    ))]));
}
