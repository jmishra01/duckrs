use pintail_common::serializer::{BinaryDeserializer, BinarySerializer, SerializationOptions};
use pintail_common::types::LogicalType;

#[test]
fn an_aggregate_state_type_keeps_its_function() {
    let layout = LogicalType::struct_type(vec![("count".into(), LogicalType::UBIGINT)]);
    let ty = LogicalType::aggregate_state_of(
        &layout,
        "stddev_samp",
        0,
        vec![LogicalType::BIGINT],
        LogicalType::DOUBLE,
    );
    let bytes = BinarySerializer::serialize(&ty, SerializationOptions::default());
    let back: LogicalType = BinaryDeserializer::deserialize(&bytes).unwrap();
    assert!(back.is_aggregate_state(), "{back:?}");
    assert_eq!(back.aggregate_state_info().unwrap().0, "stddev_samp");
    assert_eq!(back, ty);
}
