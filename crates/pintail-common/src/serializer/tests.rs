use super::*;
use crate::types::{Date, Interval, LogicalType};
use proptest::prelude::*;

fn bytes<T: Serialize>(v: &T) -> Vec<u8> {
    BinarySerializer::serialize(v, SerializationOptions::default())
}

fn round_trip<T: Serialize + Deserialize>(v: &T) -> T {
    BinaryDeserializer::deserialize(&bytes(v)).unwrap()
}

#[test]
fn varints_match_leb128() {
    let mut s = BinarySerializer::default();
    s.write_varint_signed(42);
    s.write_varint_signed(-1);
    s.write_varint_signed(64);
    s.write_varint_signed(-65);
    s.write_varint_unsigned(300);
    assert_eq!(s.into_bytes(), [0x2A, 0x7F, 0xC0, 0x00, 0xBF, 0x7F, 0xAC, 0x02]);
    for v in [0i64, 1, -1, 63, 64, -64, -65, i64::MAX, i64::MIN] {
        let mut s = BinarySerializer::default();
        s.write_varint_signed(v);
        let b = s.into_bytes();
        assert_eq!(BinaryDeserializer::new(&b).read_varint_signed(64).unwrap(), v);
    }
}

/// Byte layouts derived from DuckDB's BinarySerializer rules.
#[test]
fn type_bytes() {
    // { 100: id=13 } terminator
    assert_eq!(bytes(&LogicalType::INTEGER), [0x64, 0x00, 0x0D, 0xFF, 0xFF]);
    // { 100: id=21, 101: present, { 100: type=2, 200: width=10, 201: scale=2 } } (alias omitted)
    assert_eq!(
        bytes(&LogicalType::decimal(10, 2)),
        [
            0x64, 0x00, 0x15, 0x65, 0x00, 0x01, 0x64, 0x00, 0x02, 0xC8, 0x00, 0x0A, 0xC9, 0x00, 0x02, 0xFF, 0xFF, 0xFF,
            0xFF
        ]
    );
    // DECIMAL(4,0): scale 0 is the default, so it is omitted.
    assert_eq!(
        bytes(&LogicalType::decimal(4, 0)),
        [
            0x64, 0x00, 0x15, 0x65, 0x00, 0x01, 0x64, 0x00, 0x02, 0xC8, 0x00, 0x04, 0xFF, 0xFF, 0xFF, 0xFF
        ]
    );
}

#[test]
fn value_bytes() {
    // { 100: type INTEGER, 101: is_null=false, 102: 42 }
    assert_eq!(
        bytes(&Value::integer(42)),
        [
            0x64, 0x00, 0x64, 0x00, 0x0D, 0xFF, 0xFF, 0x65, 0x00, 0x00, 0x66, 0x00, 0x2A, 0xFF, 0xFF
        ]
    );
    // A NULL stops after is_null.
    assert_eq!(
        bytes(&Value::null(LogicalType::INTEGER)),
        [0x64, 0x00, 0x64, 0x00, 0x0D, 0xFF, 0xFF, 0x65, 0x00, 0x01, 0xFF, 0xFF]
    );
    // Strings: varint length, then bytes.
    assert_eq!(
        bytes(&Value::varchar("hi")),
        [
            0x64, 0x00, 0x64, 0x00, 0x19, 0xFF, 0xFF, 0x65, 0x00, 0x00, 0x66, 0x00, 0x02, b'h', b'i', 0xFF, 0xFF
        ]
    );
}

#[test]
fn nested_children_omit_their_types() {
    let list = Value::list(LogicalType::INTEGER, vec![Value::integer(1)]).unwrap();
    let b = bytes(&list);
    // ... 102: { 100: [count=1] { 101: false, 102: 1 } }
    let tail = [
        0x66, 0x00, 0x64, 0x00, 0x01, 0x65, 0x00, 0x00, 0x66, 0x00, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ];
    assert!(b.ends_with(&tail), "{b:02X?}");
    // Older storage versions write every child's type.
    let old = BinarySerializer::serialize(
        &list,
        SerializationOptions {
            target: StorageVersion::V1_1_0,
            ..Default::default()
        },
    );
    assert!(old.len() > b.len());
    assert_eq!(BinaryDeserializer::deserialize::<Value>(&b).unwrap(), list);
}

#[test]
fn types_round_trip() {
    let types = [
        LogicalType::VARCHAR,
        LogicalType::decimal(38, 10),
        LogicalType::list(LogicalType::list(LogicalType::DOUBLE)),
        LogicalType::struct_type(vec![
            ("a".into(), LogicalType::INTEGER),
            ("b c".into(), LogicalType::VARCHAR),
        ]),
        LogicalType::map(LogicalType::VARCHAR, LogicalType::BIGINT),
        LogicalType::array(LogicalType::FLOAT, 3),
        LogicalType::union_type(vec![
            ("n".into(), LogicalType::INTEGER),
            ("s".into(), LogicalType::VARCHAR),
        ]),
        LogicalType::enum_type(vec!["x".into(), "y".into()]),
        LogicalType::INTEGER.with_alias("myint"),
        LogicalType::struct_type(vec![(String::new(), LogicalType::INTEGER)]).with_id(LogicalTypeId::Tuple),
    ];
    for ty in types {
        assert_eq!(round_trip(&ty), ty, "{ty}");
    }
}

#[test]
fn values_round_trip() {
    let values = [
        Value::boolean(true),
        Value::tinyint(-5),
        Value::hugeint(i128::MIN),
        Value::uhugeint(u128::MAX),
        Value::double(-0.5),
        Value::float(f32::MAX),
        Value::varchar("héllo"),
        Value::blob(b"\x00a\\'\xff".to_vec()),
        Value::interval(Interval::new(-3, 7, 123_456_789)),
        Value::date(Date(-5)),
        Value::decimal(-12345, 10, 2).unwrap(),
        Value::struct_value(vec![
            ("a".into(), Value::integer(1)),
            (
                "b".into(),
                Value::list(
                    LogicalType::VARCHAR,
                    vec![Value::varchar("x"), Value::null(LogicalType::VARCHAR)],
                )
                .unwrap(),
            ),
        ]),
        Value::null(LogicalType::list(LogicalType::INTEGER)),
    ];
    for v in values {
        assert_eq!(round_trip(&v), v, "{v:?}");
    }
}

#[test]
fn errors_name_the_field() {
    let b = bytes(&LogicalType::INTEGER);
    let mut d = BinaryDeserializer::new(&b);
    let err = d.read_property::<u8>(101, "id").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Serialization Error: Failed to deserialize: field id mismatch, expected: 101, got: 100"
    );
    assert!(BinaryDeserializer::deserialize::<LogicalType>(&b[..3]).is_err());
}

proptest! {
    #[test]
    fn primitive_values_round_trip(i in any::<i64>(), u in any::<u64>(), h in any::<i128>(), s in ".{0,40}", f in any::<f64>()) {
        prop_assert_eq!(round_trip(&Value::bigint(i)), Value::bigint(i));
        prop_assert_eq!(round_trip(&Value::ubigint(u)), Value::ubigint(u));
        prop_assert_eq!(round_trip(&Value::hugeint(h)), Value::hugeint(h));
        prop_assert_eq!(round_trip(&Value::varchar(s.clone())), Value::varchar(s));
        let back = round_trip(&Value::double(f));
        prop_assert_eq!(bytes(&back), bytes(&Value::double(f)));
    }
}
