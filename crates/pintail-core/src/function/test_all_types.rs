// Ported from duckdb/src/function/table/system/test_all_types.cpp and the extremes of
// `Value::MinimumValue` and `Value::MaximumValue` in duckdb/src/common/types/value.cpp
// @ rust-port-baseline
//
//! The columns of `test_all_types()`. Most of the corpus' tests of the type system start here.

use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::BoundExpr;
use pintail_common::Result;
use pintail_common::types::Interval;
use pintail_common::types::bignum;
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::{Value, ValueData};

const MICROS_PER_SEC: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SEC;
const NANOS_PER_DAY: i64 = MICROS_PER_DAY * 1000;

/// One column: its name and the values of the first two rows, the third being NULL.
pub struct TestColumn {
    pub name: String,
    pub min: Value,
    pub max: Value,
}

const DUCKS: &str = "\u{1F986}\u{1F986}\u{1F986}\u{1F986}\u{1F986}\u{1F986}";
/// `Date::FromDate(Date::DATE_MIN_YEAR, ...)`, as days since the epoch.
const DATE_MIN: i32 = -2_147_483_646;
const DATE_MAX: i32 = 2_147_483_646;
/// `Timestamp::MIN_YEAR` etc., as microseconds since the epoch.
const TIMESTAMP_MIN: i64 = -9_223_372_022_400_000_000;
const TIMESTAMP_MAX: i64 = i64::MAX - 1;
const BIGNUM_MAX: &str = "179769313486231570814527423731704356798070567525844996598917476803157260780028538760589558632766878171540458953514382464234321326889464182768467546703537516986049910576551282076245490090389328944075868508455133942304583236903222948165808559332123348274797826204144723168738177180919299881250404026184124858368";
/// The WKT of the largest GEOMETRY of the C++ file.
const GEOMETRY_MAX: &str = "GEOMETRYCOLLECTION (POINT (1 2), POINT EMPTY, LINESTRING (0 0, 1 1), LINESTRING EMPTY, POLYGON ((0 0, 0 1, 1 1, 1 0, 0 0)), POLYGON EMPTY, MULTIPOINT (5 6, EMPTY), MULTILINESTRING ((0 0, 1 1), EMPTY, (2 2, 3 3), EMPTY), MULTILINESTRING EMPTY, MULTIPOLYGON (((0 0, 0 1, 1 1, 1 0, 0 0)), EMPTY, ((0 0, 0 2, 2 2, 2 0, 0 0)), EMPTY), MULTIPOLYGON EMPTY, GEOMETRYCOLLECTION (POINT (5 6)), GEOMETRYCOLLECTION EMPTY)";

fn wkb(wkt: &str) -> Vec<u8> {
    pintail_common::geometry::from_wkt(wkt).expect("valid WKT")
}

fn raw(id: LogicalTypeId, data: ValueData) -> Result<Value> {
    Value::new(LogicalType::new(id), data)
}

fn from_text(ty: &LogicalType, text: &str) -> Result<Value> {
    evaluate_scalar(&BoundExpr::constant(Value::varchar(text)).cast_to(ty, false))
}

fn list_of(child: &LogicalType, items: &[Option<&str>]) -> Result<Value> {
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        values.push(match item {
            Some(text) => from_text(child, text)?,
            None => Value::null(child.clone()),
        });
    }
    Value::list(child.clone(), values)
}

fn array_of(child: &LogicalType, items: Vec<Value>) -> Result<Value> {
    Value::new(
        LogicalType::array(child.clone(), items.len() as u32),
        ValueData::List(items),
    )
}

fn struct_of(fields: Vec<(&str, Value)>) -> Result<Value> {
    let children = fields
        .iter()
        .map(|(n, v)| (n.to_string(), v.logical_type().clone()))
        .collect();
    let values = fields.into_iter().map(|(_, v)| v).collect();
    Value::new(LogicalType::struct_type(children), ValueData::Struct(values))
}

fn enum_of(values: Vec<String>) -> (Value, Value) {
    let last = values.len() - 1;
    let ty = LogicalType::enum_type(values);
    (Value::enum_value(&ty, 0), Value::enum_value(&ty, last))
}

pub fn test_all_types(large_enum: bool, large_bignum: bool) -> Result<Vec<TestColumn>> {
    use LogicalTypeId as Id;
    let mut columns = Vec::with_capacity(59);
    let mut push = |name: &str, min: Value, max: Value| {
        columns.push(TestColumn {
            name: name.to_string(),
            min,
            max,
        });
    };

    push("bool", Value::boolean(false), Value::boolean(true));
    push("tinyint", Value::tinyint(i8::MIN), Value::tinyint(i8::MAX));
    push("smallint", Value::smallint(i16::MIN), Value::smallint(i16::MAX));
    push("int", Value::integer(i32::MIN), Value::integer(i32::MAX));
    push("bigint", Value::bigint(i64::MIN), Value::bigint(i64::MAX));
    push("hugeint", Value::hugeint(i128::MIN), Value::hugeint(i128::MAX));
    push("uhugeint", Value::uhugeint(0), Value::uhugeint(u128::MAX));
    push("utinyint", Value::utinyint(0), Value::utinyint(u8::MAX));
    push("usmallint", Value::usmallint(0), Value::usmallint(u16::MAX));
    push("uint", Value::uinteger(0), Value::uinteger(u32::MAX));
    push("ubigint", Value::ubigint(0), Value::ubigint(u64::MAX));

    let (min_bignum, max_bignum) = if large_bignum {
        let magnitude = vec![0xFF; bignum::MAX_DATA_SIZE];
        (
            bignum::from_magnitude(&magnitude, true),
            bignum::from_magnitude(&magnitude, false),
        )
    } else {
        let max = bignum::from_str(BIGNUM_MAX.as_bytes()).expect("a valid BIGNUM");
        (bignum::negate(&max), max)
    };
    push(
        "bignum",
        raw(Id::Bignum, ValueData::Bytes(min_bignum))?,
        raw(Id::Bignum, ValueData::Bytes(max_bignum))?,
    );

    let time_tz = LogicalType::new(Id::TimeTz);
    let nanos_min = i64::MIN / NANOS_PER_DAY * NANOS_PER_DAY;
    push(
        "date",
        raw(Id::Date, ValueData::Int32(DATE_MIN))?,
        raw(Id::Date, ValueData::Int32(DATE_MAX))?,
    );
    push(
        "time",
        raw(Id::Time, ValueData::Int64(0))?,
        raw(Id::Time, ValueData::Int64(MICROS_PER_DAY))?,
    );
    push(
        "timestamp",
        raw(Id::Timestamp, ValueData::Int64(TIMESTAMP_MIN))?,
        raw(Id::Timestamp, ValueData::Int64(TIMESTAMP_MAX))?,
    );
    push(
        "timestamp_s",
        raw(Id::TimestampSec, ValueData::Int64(TIMESTAMP_MIN / MICROS_PER_SEC))?,
        raw(Id::TimestampSec, ValueData::Int64(TIMESTAMP_MAX / MICROS_PER_SEC))?,
    );
    push(
        "timestamp_ms",
        raw(Id::TimestampMs, ValueData::Int64(TIMESTAMP_MIN / 1000))?,
        raw(Id::TimestampMs, ValueData::Int64(TIMESTAMP_MAX / 1000))?,
    );
    push(
        "timestamp_ns",
        raw(Id::TimestampNs, ValueData::Int64(nanos_min))?,
        raw(Id::TimestampNs, ValueData::Int64(i64::MAX - 1))?,
    );
    push(
        "time_tz",
        from_text(&time_tz, "00:00:00+15:59:59")?,
        from_text(&time_tz, "24:00:00-15:59:59")?,
    );
    push(
        "timestamp_tz",
        raw(Id::TimestampTz, ValueData::Int64(TIMESTAMP_MIN))?,
        raw(Id::TimestampTz, ValueData::Int64(TIMESTAMP_MAX))?,
    );
    push(
        "timestamp_tz_ns",
        raw(Id::TimestampTzNs, ValueData::Int64(nanos_min))?,
        raw(Id::TimestampTzNs, ValueData::Int64(i64::MAX - 1))?,
    );

    push("float", Value::float(f32::MIN), Value::float(f32::MAX));
    push("double", Value::double(f64::MIN), Value::double(f64::MAX));
    for (name, width, scale) in [
        ("dec_4_1", 4, 1),
        ("dec_9_4", 9, 4),
        ("dec_18_6", 18, 6),
        ("dec38_10", 38, 10),
    ] {
        let extreme = 10_i128.pow(u32::from(width)) - 1;
        push(
            name,
            Value::decimal(-extreme, width, scale)?,
            Value::decimal(extreme, width, scale)?,
        );
    }
    push("uuid", Value::uuid(i128::MIN), Value::uuid(i128::MAX));
    push(
        "interval",
        Value::interval(Interval {
            months: 0,
            days: 0,
            micros: 0,
        }),
        Value::interval(Interval {
            months: 999,
            days: 999,
            micros: 999_999_999,
        }),
    );

    push("varchar", Value::varchar(DUCKS), Value::varchar("goo\0se"));
    push(
        "blob",
        Value::blob(&b"thisisalongblob\x00withnullbytes"[..]),
        Value::blob(&b"\x00\x00\x00a"[..]),
    );
    let bit = LogicalType::new(Id::Bit);
    push(
        "bit",
        from_text(&bit, "0010001001011100010101011010111")?,
        from_text(&bit, "10101")?,
    );

    let (min, max) = enum_of(vec!["DUCK_DUCK_ENUM".into(), "GOOSE".into()]);
    push("small_enum", min, max);
    let (min, max) = enum_of((0..300).map(|i| format!("enum_{i}")).collect());
    push("medium_enum", min, max);
    let (min, max) = if large_enum {
        enum_of((0..70_000).map(|i| format!("enum_{i}")).collect())
    } else {
        enum_of(vec!["enum_0".into(), "enum_69999".into()])
    };
    push("large_enum", min, max);

    let int = LogicalType::INTEGER;
    let varchar = LogicalType::VARCHAR;
    let empty_ints = list_of(&int, &[])?;
    let ints = list_of(&int, &[Some("42"), Some("999"), None, None, Some("-42")])?;
    let int_list = ints.logical_type().clone();
    push("int_array", empty_ints.clone(), ints.clone());
    push(
        "double_array",
        list_of(&LogicalType::DOUBLE, &[])?,
        list_of(
            &LogicalType::DOUBLE,
            &[Some("42"), Some("nan"), Some("inf"), Some("-inf"), None, Some("-42")],
        )?,
    );
    push(
        "date_array",
        list_of(&LogicalType::DATE, &[])?,
        list_of(
            &LogicalType::DATE,
            &[
                Some("1970-01-01"),
                Some("infinity"),
                Some("-infinity"),
                None,
                Some("2022-05-12"),
            ],
        )?,
    );
    push(
        "timestamp_array",
        list_of(&LogicalType::TIMESTAMP, &[])?,
        list_of(
            &LogicalType::TIMESTAMP,
            &[
                Some("1970-01-01 00:00:00"),
                Some("infinity"),
                Some("-infinity"),
                None,
                Some("2022-05-12 16:23:45"),
            ],
        )?,
    );
    let timestamp_tz = LogicalType::new(Id::TimestampTz);
    push(
        "timestamptz_array",
        list_of(&timestamp_tz, &[])?,
        list_of(
            &timestamp_tz,
            &[
                Some("1970-01-01 00:00:00+00"),
                Some("infinity"),
                Some("-infinity"),
                None,
                Some("2022-05-12 16:23:45-07"),
            ],
        )?,
    );
    let empty_varchars = list_of(&varchar, &[])?;
    let varchars = list_of(&varchar, &[Some(DUCKS), Some("goose"), None, Some("")])?;
    push("varchar_array", empty_varchars, varchars.clone());
    push(
        "nested_int_array",
        Value::list(int_list.clone(), vec![])?,
        Value::list(
            int_list.clone(),
            vec![
                empty_ints.clone(),
                ints.clone(),
                Value::null(int_list.clone()),
                empty_ints.clone(),
                ints.clone(),
            ],
        )?,
    );

    let null_struct = struct_of(vec![
        ("a", Value::null(int.clone())),
        ("b", Value::null(varchar.clone())),
    ])?;
    let duck_struct = struct_of(vec![("a", Value::integer(42)), ("b", Value::varchar(DUCKS))])?;
    let struct_type = duck_struct.logical_type().clone();
    push("struct", null_struct.clone(), duck_struct.clone());
    let empty_struct = struct_of(vec![])?;
    push("empty_struct", empty_struct.clone(), empty_struct);
    push(
        "struct_of_arrays",
        struct_of(vec![
            ("a", Value::null(int_list.clone())),
            ("b", Value::null(varchars.logical_type().clone())),
        ])?,
        struct_of(vec![("a", ints.clone()), ("b", varchars)])?,
    );
    push(
        "array_of_structs",
        Value::list(struct_type.clone(), vec![])?,
        Value::list(
            struct_type.clone(),
            vec![
                null_struct.clone(),
                duck_struct.clone(),
                Value::null(struct_type.clone()),
            ],
        )?,
    );

    let map = LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR);
    let entry =
        |key: &str, value: &str| struct_of(vec![("key", Value::varchar(key)), ("value", Value::varchar(value))]);
    push(
        "map",
        Value::new(map.clone(), ValueData::List(vec![]))?,
        Value::new(
            map,
            ValueData::List(vec![entry("key1", DUCKS)?, entry("key2", "goose")?]),
        )?,
    );

    let union = LogicalType::union_type(vec![
        ("name".into(), LogicalType::VARCHAR),
        ("age".into(), LogicalType::SMALLINT),
    ]);
    push(
        "union",
        Value::new(
            union.clone(),
            ValueData::Struct(vec![
                Value::utinyint(0),
                Value::varchar("Frank"),
                Value::null(LogicalType::SMALLINT),
            ]),
        )?,
        Value::new(
            union,
            ValueData::Struct(vec![
                Value::utinyint(1),
                Value::null(LogicalType::VARCHAR),
                Value::smallint(5),
            ]),
        )?,
    );

    let min_ints = array_of(
        &int,
        vec![Value::null(int.clone()), Value::integer(2), Value::integer(3)],
    )?;
    let max_ints = array_of(&int, vec![Value::integer(4), Value::integer(5), Value::integer(6)])?;
    let int_array = min_ints.logical_type().clone();
    push("fixed_int_array", min_ints.clone(), max_ints.clone());
    let min_varchars = array_of(
        &varchar,
        vec![Value::varchar("a"), Value::null(varchar.clone()), Value::varchar("c")],
    )?;
    let max_varchars = array_of(
        &varchar,
        vec![Value::varchar("d"), Value::varchar("e"), Value::varchar("f")],
    )?;
    let varchar_array = min_varchars.logical_type().clone();
    push("fixed_varchar_array", min_varchars.clone(), max_varchars.clone());
    push(
        "fixed_nested_int_array",
        array_of(
            &int_array,
            vec![min_ints.clone(), Value::null(int_array.clone()), min_ints.clone()],
        )?,
        array_of(&int_array, vec![max_ints.clone(), min_ints.clone(), max_ints.clone()])?,
    );
    push(
        "fixed_nested_varchar_array",
        array_of(
            &varchar_array,
            vec![
                min_varchars.clone(),
                Value::null(varchar_array.clone()),
                min_varchars.clone(),
            ],
        )?,
        array_of(
            &varchar_array,
            vec![max_varchars.clone(), min_varchars.clone(), max_varchars.clone()],
        )?,
    );
    push(
        "fixed_struct_array",
        array_of(
            &struct_type,
            vec![null_struct.clone(), duck_struct.clone(), null_struct.clone()],
        )?,
        array_of(
            &struct_type,
            vec![duck_struct.clone(), null_struct, duck_struct.clone()],
        )?,
    );
    push(
        "struct_of_fixed_array",
        struct_of(vec![("a", min_ints.clone()), ("b", min_varchars)])?,
        struct_of(vec![("a", max_ints.clone()), ("b", max_varchars)])?,
    );
    push(
        "fixed_array_of_int_list",
        array_of(&int_list, vec![empty_ints.clone(), ints.clone(), empty_ints.clone()])?,
        array_of(&int_list, vec![ints.clone(), empty_ints, ints])?,
    );
    push(
        "list_of_fixed_int_array",
        Value::list(
            int_array.clone(),
            vec![min_ints.clone(), max_ints.clone(), min_ints.clone()],
        )?,
        Value::list(int_array, vec![max_ints.clone(), min_ints, max_ints])?,
    );

    push(
        "time_ns",
        raw(Id::TimeNs, ValueData::Int64(0))?,
        raw(Id::TimeNs, ValueData::Int64(NANOS_PER_DAY))?,
    );
    push(
        "geometry",
        raw(Id::Geometry, ValueData::Bytes(wkb("POINT EMPTY")))?,
        raw(Id::Geometry, ValueData::Bytes(wkb(GEOMETRY_MAX)))?,
    );
    let tuple = LogicalType::tuple(vec![LogicalType::INTEGER, LogicalType::VARCHAR]);
    push(
        "tuple",
        Value::new(
            tuple.clone(),
            ValueData::Struct(vec![Value::null(int), Value::null(varchar)]),
        )?,
        Value::new(
            tuple,
            ValueData::Struct(vec![Value::integer(42), Value::varchar(DUCKS)]),
        )?,
    );
    Ok(columns)
}
