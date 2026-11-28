use super::*;
use crate::string_cast::format_blob;
use crate::types::LogicalType;
use crate::value::{Value, ValueData};
use proptest::prelude::*;

fn key(values: &[(Value, &str)]) -> Vec<u8> {
    let vectors: Vec<Vector> = values.iter().map(|(v, _)| Vector::constant(v)).collect();
    let columns: Vec<(&Vector, OrderModifiers)> = vectors
        .iter()
        .zip(values)
        .map(|(v, (_, m))| (v, OrderModifiers::parse(m).unwrap()))
        .collect();
    create_sort_keys(&columns, 1).unwrap().remove(0)
}

/// DuckDB returns keys of up to 8 bytes as a BIGINT: the bytes left-aligned, big-endian.
fn as_bigint(key: &[u8]) -> u64 {
    assert!(key.len() <= 8);
    let mut b = [0u8; 8];
    b[..key.len()].copy_from_slice(key);
    u64::from_be_bytes(b)
}

/// Expected values come from `create_sort_key(...)` in DuckDB.
#[test]
fn keys_match_duckdb() {
    let i = |v| Value::integer(v);
    let asc = "ASC NULLS LAST";
    assert_eq!(as_bigint(&key(&[(i(42), asc)])), 108_086_391_761_534_976);
    assert_eq!(as_bigint(&key(&[(i(-1), asc)])), 108_086_391_040_114_688);
    assert_eq!(
        as_bigint(&key(&[(Value::null(LogicalType::INTEGER), asc)])),
        144_115_188_075_855_872
    );
    assert_eq!(
        as_bigint(&key(&[(Value::null(LogicalType::INTEGER), "ASC NULLS FIRST")])),
        72_057_594_037_927_936
    );
    assert_eq!(as_bigint(&key(&[(i(42), "DESC NULLS LAST")])), 108_086_390_335_471_616);
    assert_eq!(as_bigint(&key(&[(Value::boolean(true), asc)])), 72_339_069_014_638_592);
    assert_eq!(
        format_blob(&key(&[(Value::double(1.5), asc)])),
        "\\x01\\xBF\\xF8\\x00\\x00\\x00\\x00\\x00\\x00"
    );
    assert_eq!(
        format_blob(&key(&[(Value::double(-1.5), asc)])),
        "\\x01@\\x07\\xFF\\xFF\\xFF\\xFF\\xFF\\xFF"
    );
    assert_eq!(as_bigint(&key(&[(Value::float(1.5), asc)])), 126_030_420_822_196_224);
    assert_eq!(format_blob(&key(&[(Value::varchar("abc"), asc)])), "\\x01bcd\\x00");
    assert_eq!(
        format_blob(&key(&[(Value::varchar("abc"), "DESC NULLS FIRST")])),
        "\\x02\\x9D\\x9C\\x9B\\xFF"
    );
    assert_eq!(
        format_blob(&key(&[(Value::blob(b"a\x00b".to_vec()), asc)])),
        "\\x01a\\x01\\x00b\\x00"
    );
    assert_eq!(
        format_blob(&key(&[(Value::hugeint(42), asc)])),
        "\\x01\\x80\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00*"
    );
    assert_eq!(
        format_blob(&key(&[(Value::interval(Interval::new(0, 1, 0)), asc)])),
        "\\x01\\x80\\x00\\x00\\x00\\x80\\x00\\x00\\x01\\x80\\x00\\x00\\x00\\x00\\x00\\x00\\x00"
    );
    let list = Value::list(LogicalType::INTEGER, vec![i(1), Value::null(LogicalType::INTEGER)]).unwrap();
    assert_eq!(
        format_blob(&key(&[(list, asc)])),
        "\\x01\\x01\\x80\\x00\\x00\\x01\\x02\\x00"
    );
    let empty = Value::list(LogicalType::INTEGER, vec![]).unwrap();
    assert_eq!(format_blob(&key(&[(empty, "DESC NULLS LAST")])), "\\x01\\xFF");
    let s = Value::struct_value(vec![("a".into(), i(1)), ("b".into(), Value::varchar("x"))]);
    assert_eq!(
        format_blob(&key(&[(s, asc)])),
        "\\x01\\x01\\x80\\x00\\x00\\x01\\x01y\\x00"
    );
    let null_struct = Value::null(LogicalType::struct_type(vec![("a".into(), LogicalType::INTEGER)]));
    assert_eq!(format_blob(&key(&[(null_struct, asc)])), "\\x02\\x02");
    let arr = Value::new(
        LogicalType::array(LogicalType::INTEGER, 2),
        ValueData::List(vec![i(1), i(2)]),
    )
    .unwrap();
    assert_eq!(
        format_blob(&key(&[(arr, asc)])),
        "\\x01\\x01\\x80\\x00\\x00\\x01\\x01\\x80\\x00\\x00\\x02\\x00"
    );
    assert_eq!(
        format_blob(&key(&[(i(42), asc), (Value::varchar("x"), "DESC NULLS FIRST")])),
        "\\x01\\x80\\x00\\x00*\\x02\\x86\\xFF"
    );
    assert_eq!(as_bigint(&key(&[(Value::utinyint(42), asc)])), 83_879_543_059_775_488);
    assert!(OrderModifiers::parse("sideways").is_err());
}

fn sql_order(a: &Option<f64>, b: &Option<f64>, m: OrderModifiers) -> std::cmp::Ordering {
    use crate::comparison::DuckOrd;
    use std::cmp::Ordering::*;
    match (a, b) {
        (None, None) => Equal,
        (None, _) => {
            if m.nulls == NullOrder::NullsFirst {
                Less
            } else {
                Greater
            }
        }
        (_, None) => {
            if m.nulls == NullOrder::NullsFirst {
                Greater
            } else {
                Less
            }
        }
        (Some(x), Some(y)) => {
            let o = x.duck_cmp(y);
            if m.order == OrderType::Descending {
                o.reverse()
            } else {
                o
            }
        }
    }
}

fn modifiers() -> impl Strategy<Value = OrderModifiers> {
    (any::<bool>(), any::<bool>()).prop_map(|(desc, first)| {
        OrderModifiers::new(
            if desc {
                OrderType::Descending
            } else {
                OrderType::Ascending
            },
            if first {
                NullOrder::NullsFirst
            } else {
                NullOrder::NullsLast
            },
        )
    })
}

proptest! {
    /// Key order equals SQL order for doubles (incl. NaN, ±0, ±inf) and strings.
    #[test]
    fn key_order_matches_sql_order(
        a in prop::option::of(prop_oneof![any::<f64>(), Just(f64::NAN), Just(-0.0), Just(f64::INFINITY)]),
        b in prop::option::of(prop_oneof![any::<f64>(), Just(f64::NAN), Just(0.0), Just(f64::NEG_INFINITY)]),
        sa in ".{0,10}", sb in ".{0,10}",
        m in modifiers(),
    ) {
        let v = |x: &Option<f64>| x.map_or(Value::null(LogicalType::DOUBLE), Value::double);
        let name = |m: OrderModifiers| format!("{} {}",
            if m.order == OrderType::Ascending { "ASC" } else { "DESC" },
            if m.nulls == NullOrder::NullsFirst { "NULLS FIRST" } else { "NULLS LAST" });
        let (ka, kb) = (key(&[(v(&a), &name(m))]), key(&[(v(&b), &name(m))]));
        prop_assert_eq!(ka.cmp(&kb), sql_order(&a, &b, m));
        let (ks, kt) = (key(&[(Value::varchar(sa.clone()), &name(m))]), key(&[(Value::varchar(sb.clone()), &name(m))]));
        let expected = if m.order == OrderType::Descending { sb.as_bytes().cmp(sa.as_bytes()) } else { sa.as_bytes().cmp(sb.as_bytes()) };
        prop_assert_eq!(ks.cmp(&kt), expected);
    }

    /// Lists compare element-wise, shorter first (ASC), like DuckDB's list ordering.
    #[test]
    fn list_keys_order_lexicographically(a in prop::collection::vec(-5i32..5, 0..4), b in prop::collection::vec(-5i32..5, 0..4)) {
        let lv = |v: &Vec<i32>| Value::list(LogicalType::INTEGER, v.iter().map(|x| Value::integer(*x)).collect()).unwrap();
        let (ka, kb) = (key(&[(lv(&a), "ASC NULLS LAST")]), key(&[(lv(&b), "ASC NULLS LAST")]));
        prop_assert_eq!(ka.cmp(&kb), a.cmp(&b));
        let (da, db) = (key(&[(lv(&a), "DESC NULLS LAST")]), key(&[(lv(&b), "DESC NULLS LAST")]));
        prop_assert_eq!(da.cmp(&db), b.cmp(&a));
    }
}

#[test]
fn sorts_a_collection() {
    let types = [LogicalType::INTEGER, LogicalType::VARCHAR];
    let mut cdc = ColumnDataCollection::new(types.to_vec());
    let mut chunk = DataChunk::with_capacity(&types, 5000);
    for i in 0..5000 {
        let v = (i * 7919) % 5000;
        let n = if v % 10 == 0 {
            Value::null(LogicalType::INTEGER)
        } else {
            Value::integer(v % 100)
        };
        chunk.append_row(&[n, Value::varchar(format!("row {v:05}"))]).unwrap();
    }
    cdc.append(&chunk);
    let orders = [
        (0, OrderModifiers::new(OrderType::Descending, NullOrder::NullsFirst)),
        (1, OrderModifiers::ASC_NULLS_LAST),
    ];
    let sorted = sort_collection(&cdc, &orders).unwrap();
    assert_eq!(sorted.len(), 5000);
    let rows: Vec<Vec<Value>> = (0..5000).map(|r| sorted.row(r)).collect();
    // NULLs first, then integers descending, ties broken by the string ascending.
    assert!(rows[..500].iter().all(|r| r[0].is_null()));
    for w in rows[500..].windows(2) {
        let (a, b) = (w[0][0].as_i128().unwrap(), w[1][0].as_i128().unwrap());
        assert!(a > b || (a == b && w[0][1].try_to_string().unwrap() < w[1][1].try_to_string().unwrap()));
    }
    let sel = sort_chunk(&chunk, &orders).unwrap();
    assert_eq!(chunk.get_value(1, sel.get(500)), rows[500][1]);
}

fn row_key_bytes(columns: &[Vector], count: usize) -> Vec<Vec<u8>> {
    let references: Vec<&Vector> = columns.iter().collect();
    let keys = create_row_keys(&references, count).unwrap();
    let bytes: Vec<Vec<u8>> = keys.iter().map(|key| key.as_bytes().to_vec()).collect();
    // the keys in one buffer are those keys
    let buffer = create_row_key_buffer(&references, count).unwrap();
    assert_eq!(buffer.len(), count);
    for (row, key) in bytes.iter().enumerate() {
        assert_eq!(buffer.get(row).0, &key[..]);
    }
    bytes
}

fn sort_key_bytes(columns: &[Vector], count: usize) -> Vec<Vec<u8>> {
    let columns: Vec<(&Vector, OrderModifiers)> = columns.iter().map(|v| (v, OrderModifiers::ASC_NULLS_LAST)).collect();
    create_sort_keys(&columns, count).unwrap()
}

#[test]
fn row_keys_are_sort_keys() {
    let values = |values: Vec<Value>| {
        let ty = values[0].logical_type().clone();
        let mut vector = Vector::new(ty, values.len());
        vector.prepare_flat(values.len());
        for (row, value) in values.iter().enumerate() {
            vector.set_value(row, value).unwrap();
        }
        vector
    };
    let integers = values(vec![
        Value::integer(-1),
        Value::null(LogicalType::INTEGER),
        Value::integer(i32::MAX),
    ]);
    let bigints = values(vec![
        Value::bigint(i64::MIN),
        Value::bigint(0),
        Value::null(LogicalType::BIGINT),
    ]);
    let doubles = values(vec![Value::double(-0.0), Value::double(0.0), Value::double(f64::NAN)]);
    let strings = values(vec![
        Value::varchar("a".to_string()),
        Value::null(LogicalType::VARCHAR),
        Value::varchar("x".repeat(100)),
    ]);
    // each type written a column at a time, and one that is encoded row by row
    let more = [
        values(vec![
            Value::boolean(true),
            Value::null(LogicalType::BOOLEAN),
            Value::boolean(false),
        ]),
        values(vec![
            Value::tinyint(-128),
            Value::tinyint(127),
            Value::null(LogicalType::TINYINT),
        ]),
        values(vec![
            Value::smallint(-2),
            Value::null(LogicalType::SMALLINT),
            Value::smallint(300),
        ]),
        values(vec![
            Value::hugeint(i128::MIN),
            Value::hugeint(-1),
            Value::hugeint(1 << 100),
        ]),
        values(vec![
            Value::utinyint(0),
            Value::utinyint(255),
            Value::null(LogicalType::UTINYINT),
        ]),
        values(vec![Value::ubigint(u64::MAX), Value::ubigint(7), Value::ubigint(0)]),
        values(vec![Value::float(-0.0), Value::float(f32::NAN), Value::float(-1.5)]),
        values(vec![
            Value::list(LogicalType::INTEGER, vec![Value::integer(1), Value::integer(2)]).unwrap(),
            Value::null(LogicalType::list(LogicalType::INTEGER)),
            Value::list(LogicalType::INTEGER, vec![]).unwrap(),
        ]),
    ];
    for column in &more {
        assert_eq!(
            row_key_bytes(std::slice::from_ref(column), 3),
            sort_key_bytes(std::slice::from_ref(column), 3)
        );
    }
    let mixed: Vec<Vector> = more
        .iter()
        .cloned()
        .chain([strings.clone(), integers.clone()])
        .collect();
    assert_eq!(row_key_bytes(&mixed, 3), sort_key_bytes(&mixed, 3));
    for columns in [
        vec![integers.clone()],
        vec![bigints.clone(), integers.clone()],
        vec![doubles.clone()],
        vec![strings.clone()],
        // across the size that stays inline: 9 bytes per BIGINT, and a string that spills over
        vec![bigints.clone(), bigints.clone(), bigints.clone()],
        vec![bigints.clone(), bigints.clone(), bigints.clone(), bigints.clone()],
        vec![integers.clone(), strings.clone(), bigints.clone()],
    ] {
        assert_eq!(row_key_bytes(&columns, 3), sort_key_bytes(&columns, 3));
    }
    // a selection and a constant read the rows they stand for
    let mut sliced = bigints.clone();
    sliced.slice(&SelectionVector::from_vec(vec![2, 0]), 2);
    let columns = [sliced, Vector::constant(&Value::integer(7))];
    assert_eq!(row_key_bytes(&columns, 2), sort_key_bytes(&columns, 2));
    // -0.0 and 0.0 are one group
    let keys = row_key_bytes(std::slice::from_ref(&doubles), 3);
    assert_eq!(keys[0], keys[1]);
}

#[test]
fn row_keys_compare_by_their_bytes() {
    use std::hash::{BuildHasher, BuildHasherDefault};
    let hash = |key: &RowKey| BuildHasherDefault::<KeyHasher>::default().hash_one(key);
    let of = |length: usize| RowKey::from_bytes(&vec![7; length]);
    for length in [0, 1, 8, 29, 30, 31, 100] {
        assert_eq!(of(length).as_bytes().len(), length);
        assert_eq!(of(length), of(length));
        assert_eq!(hash(&of(length)), hash(&of(length)));
    }
    assert_ne!(of(30), of(31));
    // a key is in here up to the inline size, and past it on the heap
    assert!(matches!(of(30), RowKey::Inline { .. }));
    assert!(matches!(of(31), RowKey::Heap(_)));
    // the bytes of a key find it in a map, as the key does
    let map: RowKeyMap<usize> = [0, 1, 30, 31, 100]
        .into_iter()
        .map(|length| (of(length), length))
        .collect();
    for length in [0, 1, 30, 31, 100] {
        assert_eq!(map.get(&KeyBytes(&vec![7; length])), Some(&length));
    }
    assert_eq!(map.get(&KeyBytes(&[7; 2])), None);
    // integers that differ in any byte spread over the low bits, which the table reads
    let low_bits: std::collections::HashSet<u64> = (0u64..1024)
        .map(|i| {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&i.to_be_bytes());
            hash(&RowKey::from_bytes(&bytes)) & 0xFFFF
        })
        .collect();
    assert!(low_bits.len() > 1000, "{}", low_bits.len());
}

proptest! {
    #[test]
    fn row_keys_agree_with_sort_keys(a in proptest::collection::vec(proptest::option::of(any::<i64>()), 1..50)) {
        let mut vector = Vector::new(LogicalType::BIGINT, a.len());
        vector.prepare_flat(a.len());
        for (row, value) in a.iter().enumerate() {
            let value = value.map_or(Value::null(LogicalType::BIGINT), Value::bigint);
            vector.set_value(row, &value).unwrap();
        }
        let columns = [vector];
        prop_assert_eq!(row_key_bytes(&columns, a.len()), sort_key_bytes(&columns, a.len()));
    }
}
