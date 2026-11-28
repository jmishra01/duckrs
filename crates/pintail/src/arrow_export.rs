// Ported from duckdb/src/common/arrow/arrow_converter.cpp and arrow_appender.cpp @ rust-port-baseline
//
//! Results as Arrow record batches (`QueryResult::FetchArrowTable`), with the types C++ gives them by default:
//! HUGEINT as DECIMAL(38, 0), VARCHAR as `Utf8`, UUID and the types Arrow has no equivalent of as their text.

use crate::{LogicalType, LogicalTypeId, QueryResult, Value};
use arrow::array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Float32Array, Float64Array, Int8Array,
    Int16Array, Int32Array, Int64Array, IntervalMonthDayNanoArray, ListArray, MapArray, StringArray, StructArray,
    Time64MicrosecondArray, TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use arrow::buffer::{NullBuffer, OffsetBuffer};
use arrow::datatypes::{DataType, Field, Fields, IntervalMonthDayNano, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use pintail_common::value::ValueData;
use pintail_common::{Error, ErrorKind, Result};
use std::sync::Arc;

/// `ArrowConverter::ToArrowSchema`'s type of a column.
pub fn arrow_type(ty: &LogicalType) -> DataType {
    use LogicalTypeId::*;
    match ty.id() {
        Boolean => DataType::Boolean,
        TinyInt => DataType::Int8,
        SmallInt => DataType::Int16,
        Integer => DataType::Int32,
        BigInt => DataType::Int64,
        UTinyInt => DataType::UInt8,
        USmallInt => DataType::UInt16,
        UInteger => DataType::UInt32,
        UBigInt => DataType::UInt64,
        HugeInt => DataType::Decimal128(38, 0),
        Float => DataType::Float32,
        Double => DataType::Float64,
        Decimal => {
            let (width, scale) = ty.decimal_properties().unwrap_or((18, 3));
            DataType::Decimal128(width, scale as i8)
        }
        Date => DataType::Date32,
        Time => DataType::Time64(TimeUnit::Microsecond),
        Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        TimestampSec => DataType::Timestamp(TimeUnit::Second, None),
        TimestampMs => DataType::Timestamp(TimeUnit::Millisecond, None),
        TimestampNs => DataType::Timestamp(TimeUnit::Nanosecond, None),
        TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        Interval => DataType::Interval(arrow::datatypes::IntervalUnit::MonthDayNano),
        Blob => DataType::Binary,
        List | Array => DataType::List(Arc::new(Field::new(
            "l",
            arrow_type(ty.child_type().unwrap_or(&LogicalType::SQLNULL)),
            true,
        ))),
        Struct => DataType::Struct(struct_fields(ty)),
        Map => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let fields = struct_fields(&entry);
            let fields = Fields::from(
                fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| f.as_ref().clone().with_nullable(i != 0))
                    .collect::<Vec<_>>(),
            );
            DataType::Map(Arc::new(Field::new("entries", DataType::Struct(fields), false)), false)
        }
        SqlNull => DataType::Null,
        _ => DataType::Utf8,
    }
}

fn struct_fields(ty: &LogicalType) -> Fields {
    ty.struct_children()
        .iter()
        .map(|(name, child)| Field::new(name, arrow_type(child), true))
        .collect()
}

/// The Arrow schema of a result (`ArrowConverter::ToArrowSchema`).
pub fn arrow_schema(result: &QueryResult) -> Schema {
    Schema::new(
        result
            .names
            .iter()
            .zip(&result.types)
            .map(|(name, ty)| Field::new(name, arrow_type(ty), true))
            .collect::<Vec<_>>(),
    )
}

fn nulls(values: &[&Value]) -> Option<NullBuffer> {
    let valid: Vec<bool> = values.iter().map(|v| !v.is_null()).collect();
    valid.iter().any(|v| !v).then(|| NullBuffer::from(valid))
}

fn integer(value: &Value) -> Option<i128> {
    match value.data() {
        ValueData::Int8(v) => Some(i128::from(*v)),
        ValueData::Int16(v) => Some(i128::from(*v)),
        ValueData::Int32(v) => Some(i128::from(*v)),
        ValueData::Int64(v) => Some(i128::from(*v)),
        ValueData::Int128(v) => Some(*v),
        ValueData::UInt8(v) => Some(i128::from(*v)),
        ValueData::UInt16(v) => Some(i128::from(*v)),
        ValueData::UInt32(v) => Some(i128::from(*v)),
        ValueData::UInt64(v) => Some(i128::from(*v)),
        ValueData::UInt128(v) => i128::try_from(*v).ok(),
        ValueData::Bool(b) => Some(i128::from(*b)),
        _ => None,
    }
}

fn float(value: &Value) -> Option<f64> {
    match value.data() {
        ValueData::Float(v) => Some(f64::from(*v)),
        ValueData::Double(v) => Some(*v),
        _ => integer(value).map(|v| v as f64),
    }
}

/// The values of one column as an array of `arrow_type(ty)` (`ArrowAppender`).
pub fn arrow_array(ty: &LogicalType, values: &[&Value]) -> Result<ArrayRef> {
    use LogicalTypeId::*;
    let ints = || values.iter().map(|v| integer(v)).collect::<Vec<_>>();
    Ok(match ty.id() {
        Boolean => Arc::new(BooleanArray::from(
            values
                .iter()
                .map(|v| match v.data() {
                    ValueData::Bool(b) => Some(*b),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        TinyInt => Arc::new(Int8Array::from(
            ints().into_iter().map(|v| v.map(|v| v as i8)).collect::<Vec<_>>(),
        )),
        SmallInt => Arc::new(Int16Array::from(
            ints().into_iter().map(|v| v.map(|v| v as i16)).collect::<Vec<_>>(),
        )),
        Integer => Arc::new(Int32Array::from(
            ints().into_iter().map(|v| v.map(|v| v as i32)).collect::<Vec<_>>(),
        )),
        BigInt => Arc::new(Int64Array::from(
            ints().into_iter().map(|v| v.map(|v| v as i64)).collect::<Vec<_>>(),
        )),
        UTinyInt => Arc::new(UInt8Array::from(
            ints().into_iter().map(|v| v.map(|v| v as u8)).collect::<Vec<_>>(),
        )),
        USmallInt => Arc::new(UInt16Array::from(
            ints().into_iter().map(|v| v.map(|v| v as u16)).collect::<Vec<_>>(),
        )),
        UInteger => Arc::new(UInt32Array::from(
            ints().into_iter().map(|v| v.map(|v| v as u32)).collect::<Vec<_>>(),
        )),
        UBigInt => Arc::new(UInt64Array::from(
            ints().into_iter().map(|v| v.map(|v| v as u64)).collect::<Vec<_>>(),
        )),
        HugeInt | Decimal => {
            let DataType::Decimal128(width, scale) = arrow_type(ty) else {
                unreachable!("a decimal")
            };
            Arc::new(
                Decimal128Array::from(ints())
                    .with_precision_and_scale(width, scale)
                    .map_err(|e| Error::new(ErrorKind::Conversion, e.to_string()))?,
            )
        }
        Float => Arc::new(Float32Array::from(
            values.iter().map(|v| float(v).map(|f| f as f32)).collect::<Vec<_>>(),
        )),
        Double => Arc::new(Float64Array::from(values.iter().map(|v| float(v)).collect::<Vec<_>>())),
        Date => Arc::new(Date32Array::from(
            ints().into_iter().map(|v| v.map(|v| v as i32)).collect::<Vec<_>>(),
        )),
        Time => Arc::new(Time64MicrosecondArray::from(
            ints().into_iter().map(|v| v.map(|v| v as i64)).collect::<Vec<_>>(),
        )),
        Timestamp => Arc::new(TimestampMicrosecondArray::from(
            ints().into_iter().map(|v| v.map(|v| v as i64)).collect::<Vec<_>>(),
        )),
        TimestampSec => Arc::new(TimestampSecondArray::from(
            ints().into_iter().map(|v| v.map(|v| v as i64)).collect::<Vec<_>>(),
        )),
        TimestampMs => Arc::new(TimestampMillisecondArray::from(
            ints().into_iter().map(|v| v.map(|v| v as i64)).collect::<Vec<_>>(),
        )),
        TimestampNs => Arc::new(TimestampNanosecondArray::from(
            ints().into_iter().map(|v| v.map(|v| v as i64)).collect::<Vec<_>>(),
        )),
        TimestampTz => Arc::new(
            TimestampMicrosecondArray::from(ints().into_iter().map(|v| v.map(|v| v as i64)).collect::<Vec<_>>())
                .with_timezone("UTC"),
        ),
        Interval => Arc::new(IntervalMonthDayNanoArray::from(
            values
                .iter()
                .map(|v| match v.data() {
                    ValueData::Interval(i) => Some(IntervalMonthDayNano::new(i.months, i.days, i.micros * 1000)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        Blob => Arc::new(BinaryArray::from(
            values
                .iter()
                .map(|v| match v.data() {
                    ValueData::Bytes(bytes) => Some(bytes.as_slice()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        List | Array => {
            let child_type = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let mut offsets = vec![0i32];
            let mut children: Vec<&Value> = Vec::new();
            for value in values {
                if let ValueData::List(items) = value.data() {
                    children.extend(items.iter());
                }
                offsets.push(children.len() as i32);
            }
            let child = arrow_array(&child_type, &children)?;
            let DataType::List(field) = arrow_type(ty) else {
                unreachable!("a list")
            };
            Arc::new(ListArray::new(
                field,
                OffsetBuffer::new(offsets.into()),
                child,
                nulls(values),
            ))
        }
        Struct => Arc::new(struct_array(ty, values)?),
        Map => {
            let entry_type = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let mut offsets = vec![0i32];
            let mut entries: Vec<&Value> = Vec::new();
            for value in values {
                if let ValueData::List(items) = value.data() {
                    entries.extend(items.iter());
                }
                offsets.push(entries.len() as i32);
            }
            let DataType::Map(field, sorted) = arrow_type(ty) else {
                unreachable!("a map")
            };
            let DataType::Struct(fields) = field.data_type().clone() else {
                unreachable!("map entries")
            };
            let entries = struct_array(&entry_type, &entries)?;
            let entries = StructArray::new(fields, entries.columns().to_vec(), None);
            Arc::new(
                MapArray::try_new(field, OffsetBuffer::new(offsets.into()), entries, nulls(values), sorted)
                    .map_err(|e| Error::new(ErrorKind::Conversion, e.to_string()))?,
            )
        }
        SqlNull => arrow::array::new_null_array(&DataType::Null, values.len()),
        _ => Arc::new(StringArray::from(
            values
                .iter()
                .map(|v| if v.is_null() { None } else { v.try_to_string().ok() })
                .collect::<Vec<_>>(),
        )),
    })
}

fn struct_array(ty: &LogicalType, values: &[&Value]) -> Result<StructArray> {
    let fields = struct_fields(ty);
    let mut columns = Vec::with_capacity(fields.len());
    for (index, (_, child_type)) in ty.struct_children().iter().enumerate() {
        let null = Value::null(child_type.clone());
        let children: Vec<&Value> = values
            .iter()
            .map(|v| match v.data() {
                ValueData::Struct(items) => items.get(index).unwrap_or(&null),
                _ => &null,
            })
            .collect();
        columns.push(arrow_array(child_type, &children)?);
    }
    if fields.is_empty() {
        return Ok(StructArray::new_empty_fields(values.len(), nulls(values)));
    }
    Ok(StructArray::new(fields, columns, nulls(values)))
}

/// `QueryResult::FetchArrowTable`: the rows of a result in record batches of up to `batch_rows` rows.
pub fn record_batches(result: &QueryResult, batch_rows: usize) -> Result<Vec<RecordBatch>> {
    let schema = Arc::new(arrow_schema(result));
    let rows = result.row_count();
    let batch_rows = batch_rows.max(1);
    let mut batches = Vec::new();
    let mut start = 0;
    while start < rows || (rows == 0 && batches.is_empty()) {
        let end = (start + batch_rows).min(rows);
        let mut columns = Vec::with_capacity(result.types.len());
        for (column, ty) in result.types.iter().enumerate() {
            let values: Vec<Value> = (start..end).map(|row| result.value(column, row)).collect();
            let values: Vec<&Value> = values.iter().collect();
            columns.push(arrow_array(ty, &values)?);
        }
        let batch = if columns.is_empty() {
            RecordBatch::try_new_with_options(
                schema.clone(),
                columns,
                &arrow::record_batch::RecordBatchOptions::new().with_row_count(Some(end - start)),
            )
        } else {
            RecordBatch::try_new(schema.clone(), columns)
        }
        .map_err(|e| Error::new(ErrorKind::Conversion, e.to_string()))?;
        batches.push(batch);
        if rows == 0 {
            break;
        }
        start = end;
    }
    Ok(batches)
}
