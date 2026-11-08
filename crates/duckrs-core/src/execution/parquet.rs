// Ported from duckdb/extension/parquet @ rust-port-baseline
//
//! Reading and writing Parquet: the plan's v1, through the `parquet` crate's Arrow reader and writer.
//! Rows pass as `Value`s between Arrow and vectors, so this is complete rather than fast, and
//! neither filters nor projections are pushed into the file.

use super::csv::hive_parts;
use super::physical::{PhysicalOperator, SourceState};
use crate::execution::parquet_writer::{Codec, ParquetFileWriter};
use crate::function::cast::{CastMode, cast_vector};
use arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use arrow::datatypes::{self as adt, DataType, Field, TimeUnit};
use duckrs_common::types::{Date, Interval, LogicalType, LogicalTypeId, Time, TimeTz, Timestamp};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use parking_lot::Mutex;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use parquet::file::statistics::Statistics;
use parquet::schema::types::Type;
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const UUID_EXTENSION: &str = "arrow.uuid";
const EXTENSION_KEY: &str = "ARROW:extension:name";

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::InvalidInput, message.to_string())
}

fn unsupported(what: impl std::fmt::Display) -> Error {
    crate::unported(format!("{what} in a Parquet file"))
}

/// What `read_parquet` found and was told.
#[derive(Debug)]
pub struct ParquetScanData {
    pub files: Vec<String>,
    /// the columns that come from the files
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub binary_as_string: bool,
    /// `int96_as`
    pub int96_as: Int96As,
    pub union_by_name: bool,
    pub file_row_number: bool,
    pub filename: bool,
    pub hive: Vec<(String, LogicalType)>,
    /// the virtual columns, after the rest
    pub virtual_columns: Vec<crate::execution::multi_file::VirtualColumn>,
}

impl ParquetScanData {
    pub fn output_types(&self) -> Vec<LogicalType> {
        let mut types = self.types.clone();
        if self.file_row_number {
            types.push(LogicalType::BIGINT);
        }
        if self.filename {
            types.push(LogicalType::VARCHAR);
        }
        types.extend(self.hive.iter().map(|(_, ty)| ty.clone()));
        types.extend(crate::execution::multi_file::virtual_types(&self.virtual_columns));
        types
    }
}

/// Runs the crate on a file that may be corrupt: it panics on some, which must stay one query's error.
fn guarded<T>(path: &str, read: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).unwrap_or_else(|panic| {
        let reason = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("the file is corrupt");
        Err(invalid(format!(
            "Invalid Parquet file \"{path}\": {reason}. Parquet file is likely corrupted"
        )))
    })
}

fn open_builder(path: &str) -> Result<ParquetRecordBatchReaderBuilder<File>> {
    let file = File::open(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
    guarded(path, || {
        ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(|e| invalid(format!("Invalid Parquet file \"{path}\": {e}")))
    })
}

/// `ParquetReader::DeriveLogicalType`, from what the crate makes of the file.
fn logical_type(field: &Field, binary_as_string: bool) -> Result<LogicalType> {
    let text_or_blob = if binary_as_string {
        LogicalType::VARCHAR
    } else {
        LogicalType::BLOB
    };
    Ok(match field.data_type() {
        DataType::Null => LogicalType::INTEGER,
        DataType::Boolean => LogicalType::BOOLEAN,
        DataType::Int8 => LogicalType::TINYINT,
        DataType::Int16 => LogicalType::SMALLINT,
        DataType::Int32 => LogicalType::INTEGER,
        DataType::Int64 => LogicalType::BIGINT,
        DataType::UInt8 => LogicalType::UTINYINT,
        DataType::UInt16 => LogicalType::USMALLINT,
        DataType::UInt32 => LogicalType::UINTEGER,
        DataType::UInt64 => LogicalType::UBIGINT,
        DataType::Float16 | DataType::Float32 => LogicalType::FLOAT,
        DataType::Float64 => LogicalType::DOUBLE,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => LogicalType::VARCHAR,
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => text_or_blob,
        DataType::FixedSizeBinary(16)
            if field
                .metadata()
                .get(EXTENSION_KEY)
                .is_some_and(|name| name == UUID_EXTENSION) =>
        {
            LogicalType::UUID
        }
        DataType::FixedSizeBinary(_) => text_or_blob,
        DataType::Date32 | DataType::Date64 => LogicalType::DATE,
        DataType::Time32(_) | DataType::Time64(_) => LogicalType::TIME,
        DataType::Timestamp(TimeUnit::Nanosecond, None) => LogicalType::new(LogicalTypeId::TimestampNs),
        DataType::Timestamp(_, None) => LogicalType::TIMESTAMP,
        DataType::Timestamp(_, Some(_)) => LogicalType::new(LogicalTypeId::TimestampTz),
        DataType::Interval(_) => LogicalType::INTERVAL,
        DataType::Decimal32(width, scale) | DataType::Decimal64(width, scale) | DataType::Decimal128(width, scale) => {
            LogicalType::decimal(*width, (*scale).max(0) as u8)
        }
        DataType::Decimal256(..) => LogicalType::DOUBLE,
        DataType::List(child) | DataType::LargeList(child) | DataType::FixedSizeList(child, _) => {
            LogicalType::list(logical_type(child, binary_as_string)?)
        }
        DataType::Struct(fields) => LogicalType::struct_type(
            fields
                .iter()
                .map(|f| Ok((f.name().clone(), logical_type(f, binary_as_string)?)))
                .collect::<Result<_>>()?,
        ),
        DataType::Map(entries, _) => {
            let DataType::Struct(fields) = entries.data_type() else {
                return Err(invalid("A MAP in a Parquet file has no key and value"));
            };
            LogicalType::map(
                logical_type(&fields[0], binary_as_string)?,
                logical_type(&fields[1], binary_as_string)?,
            )
        }
        DataType::Dictionary(_, value) => {
            logical_type(&Field::new(field.name(), (**value).clone(), true), binary_as_string)?
        }
        other => return Err(unsupported(format!("the type {other}"))),
    })
}

/// What `read_parquet` makes of INT96 columns (`int96_as`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Int96As {
    /// microseconds, the nanoseconds cut off
    #[default]
    Timestamp,
    TimestampNs,
    /// `STRUCT(date DATE, time TIME_NS)`, all of the value
    Struct,
}

/// A TIME, or with `ty` TIMETZ, a TIME in UTC (`ParquetIntToTimeTZ`).
fn time_value(micros: i64, ty: &LogicalType) -> Value {
    if ty.id() == LogicalTypeId::TimeTz {
        Value::time_tz(TimeTz::new(Time(micros), 0))
    } else {
        Value::time(Time(micros))
    }
}

/// What the crate leaves out of a top-level column's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Special {
    None,
    /// INT96 (Impala timestamps), which the crate reads as nanoseconds
    Int96,
    /// a TIME adjusted to UTC, which DuckDB reads as TIMETZ
    TimeUtc,
}

fn special_columns(builder: &ParquetRecordBatchReaderBuilder<File>) -> Vec<Special> {
    builder
        .parquet_schema()
        .root_schema()
        .get_fields()
        .iter()
        .map(|field| {
            if !field.is_primitive() {
                return Special::None;
            }
            if field.get_physical_type() == parquet::basic::Type::INT96 {
                return Special::Int96;
            }
            match field.get_basic_info().logical_type_ref() {
                Some(parquet::basic::LogicalType::Time(time)) if time.is_adjusted_to_u_t_c => Special::TimeUtc,
                _ => Special::None,
            }
        })
        .collect()
}

/// The DuckDB type of top-level column `index` of a file: INT96 by `int96_as`, the rest by [`logical_type`].
fn column_type(
    field: &Field,
    special: &[Special],
    index: usize,
    binary_as_string: bool,
    int96_as: Int96As,
) -> Result<LogicalType> {
    if special.get(index) == Some(&Special::TimeUtc) {
        return Ok(LogicalType::new(LogicalTypeId::TimeTz));
    }
    if special.get(index) == Some(&Special::Int96) {
        return Ok(match int96_as {
            Int96As::Timestamp => LogicalType::TIMESTAMP,
            Int96As::TimestampNs => LogicalType::new(LogicalTypeId::TimestampNs),
            Int96As::Struct => LogicalType::struct_type(vec![
                ("date".to_string(), LogicalType::DATE),
                ("time".to_string(), LogicalType::new(LogicalTypeId::TimeNs)),
            ]),
        });
    }
    logical_type(field, binary_as_string)
}

/// The names and types of a file's columns.
pub fn file_schema(path: &str, binary_as_string: bool, int96_as: Int96As) -> Result<(Vec<String>, Vec<LogicalType>)> {
    let builder = open_builder(path)?;
    let int96 = special_columns(&builder);
    let schema = builder.schema();
    let names = schema.fields().iter().map(|f| f.name().clone()).collect();
    let types = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| column_type(f, &int96, i, binary_as_string, int96_as))
        .collect::<Result<_>>()?;
    Ok((names, types))
}

fn utf8(bytes: &[u8]) -> Result<Value> {
    std::str::from_utf8(bytes)
        .map(Value::varchar)
        .map_err(|_| invalid("Invalid string encoding found in Parquet file: value is not valid UTF8!"))
}

fn binary_value(bytes: &[u8], ty: &LogicalType) -> Result<Value> {
    match ty.id() {
        LogicalTypeId::Varchar => utf8(bytes),
        LogicalTypeId::Uuid => {
            let raw = <[u8; 16]>::try_from(bytes).map_err(|_| invalid("A UUID in a Parquet file is not 16 bytes"))?;
            Ok(Value::uuid((u128::from_be_bytes(raw) ^ (1 << 127)) as i128))
        }
        _ => Ok(Value::blob(bytes)),
    }
}

fn timestamp_value(value: i64, unit: &TimeUnit, ty: &LogicalType) -> Result<Value> {
    if ty.id() == LogicalTypeId::Struct && *unit == TimeUnit::Nanosecond {
        // `int96_as = 'struct'`: the day, and the nanoseconds into it
        const NANOS_PER_DAY: i64 = 86_400_000_000_000;
        let date = Value::date(Date(value.div_euclid(NANOS_PER_DAY) as i32));
        let time = Value::new(
            LogicalType::new(LogicalTypeId::TimeNs),
            ValueData::Int64(value.rem_euclid(NANOS_PER_DAY)),
        )?;
        return Ok(Value::struct_value(vec![
            ("date".to_string(), date),
            ("time".to_string(), time),
        ]));
    }
    if ty.id() == LogicalTypeId::TimestampNs {
        return Value::new(ty.clone(), ValueData::Int64(value));
    }
    let micros = match unit {
        TimeUnit::Second => value.checked_mul(1_000_000),
        TimeUnit::Millisecond => value.checked_mul(1_000),
        TimeUnit::Microsecond => Some(value),
        TimeUnit::Nanosecond => Some(value.div_euclid(1_000)),
    }
    .ok_or_else(|| invalid("A timestamp in a Parquet file is out of range"))?;
    Ok(if ty.id() == LogicalTypeId::TimestampTz {
        Value::timestamp_tz(Timestamp(micros))
    } else {
        Value::timestamp(Timestamp(micros))
    })
}

fn decimal_value(unscaled: i128, ty: &LogicalType) -> Result<Value> {
    let (width, scale) = ty.decimal_properties().unwrap_or((38, 0));
    Value::decimal(unscaled, width, scale)
}

/// The value of `row` of `array`, as a `ty`.
fn array_value(array: &dyn Array, row: usize, ty: &LogicalType) -> Result<Value> {
    if array.is_null(row) {
        return Ok(Value::null(ty.clone()));
    }
    let list_items = |values: ArrayRef, child: &LogicalType| -> Result<Vec<Value>> {
        (0..values.len()).map(|i| array_value(&values, i, child)).collect()
    };
    Ok(match array.data_type() {
        DataType::Null => Value::null(ty.clone()),
        DataType::Boolean => Value::boolean(array.as_boolean().value(row)),
        DataType::Int8 => Value::tinyint(array.as_primitive::<adt::Int8Type>().value(row)),
        DataType::Int16 => Value::smallint(array.as_primitive::<adt::Int16Type>().value(row)),
        DataType::Int32 => Value::integer(array.as_primitive::<adt::Int32Type>().value(row)),
        DataType::Int64 => Value::bigint(array.as_primitive::<adt::Int64Type>().value(row)),
        DataType::UInt8 => Value::utinyint(array.as_primitive::<adt::UInt8Type>().value(row)),
        DataType::UInt16 => Value::usmallint(array.as_primitive::<adt::UInt16Type>().value(row)),
        DataType::UInt32 => Value::uinteger(array.as_primitive::<adt::UInt32Type>().value(row)),
        DataType::UInt64 => Value::ubigint(array.as_primitive::<adt::UInt64Type>().value(row)),
        DataType::Float16 => Value::float(array.as_primitive::<adt::Float16Type>().value(row).to_f32()),
        DataType::Float32 => Value::float(array.as_primitive::<adt::Float32Type>().value(row)),
        DataType::Float64 => Value::double(array.as_primitive::<adt::Float64Type>().value(row)),
        DataType::Utf8 => utf8(array.as_string::<i32>().value(row).as_bytes())?,
        DataType::LargeUtf8 => utf8(array.as_string::<i64>().value(row).as_bytes())?,
        DataType::Utf8View => utf8(array.as_string_view().value(row).as_bytes())?,
        DataType::Binary => binary_value(array.as_binary::<i32>().value(row), ty)?,
        DataType::LargeBinary => binary_value(array.as_binary::<i64>().value(row), ty)?,
        DataType::BinaryView => binary_value(array.as_binary_view().value(row), ty)?,
        DataType::FixedSizeBinary(_) => binary_value(array.as_fixed_size_binary().value(row), ty)?,
        DataType::Date32 => Value::date(Date(array.as_primitive::<adt::Date32Type>().value(row))),
        DataType::Date64 => {
            let millis = array.as_primitive::<adt::Date64Type>().value(row);
            Value::date(Date(millis.div_euclid(86_400_000) as i32))
        }
        DataType::Time32(TimeUnit::Second) => {
            let seconds = array.as_primitive::<adt::Time32SecondType>().value(row);
            time_value(i64::from(seconds) * 1_000_000, ty)
        }
        DataType::Time32(_) => {
            let millis = array.as_primitive::<adt::Time32MillisecondType>().value(row);
            time_value(i64::from(millis) * 1_000, ty)
        }
        DataType::Time64(TimeUnit::Nanosecond) => {
            time_value(array.as_primitive::<adt::Time64NanosecondType>().value(row) / 1_000, ty)
        }
        DataType::Time64(_) => time_value(array.as_primitive::<adt::Time64MicrosecondType>().value(row), ty),
        DataType::Timestamp(unit, _) => {
            let raw = match unit {
                TimeUnit::Second => array.as_primitive::<adt::TimestampSecondType>().value(row),
                TimeUnit::Millisecond => array.as_primitive::<adt::TimestampMillisecondType>().value(row),
                TimeUnit::Microsecond => array.as_primitive::<adt::TimestampMicrosecondType>().value(row),
                TimeUnit::Nanosecond => array.as_primitive::<adt::TimestampNanosecondType>().value(row),
            };
            timestamp_value(raw, unit, ty)?
        }
        DataType::Interval(adt::IntervalUnit::YearMonth) => Value::interval(Interval {
            months: array.as_primitive::<adt::IntervalYearMonthType>().value(row),
            days: 0,
            micros: 0,
        }),
        DataType::Interval(adt::IntervalUnit::DayTime) => {
            let value = array.as_primitive::<adt::IntervalDayTimeType>().value(row);
            Value::interval(Interval {
                months: 0,
                days: value.days,
                micros: i64::from(value.milliseconds) * 1_000,
            })
        }
        DataType::Interval(adt::IntervalUnit::MonthDayNano) => {
            let value = array.as_primitive::<adt::IntervalMonthDayNanoType>().value(row);
            Value::interval(Interval {
                months: value.months,
                days: value.days,
                micros: value.nanoseconds / 1_000,
            })
        }
        DataType::Decimal32(..) => {
            decimal_value(i128::from(array.as_primitive::<adt::Decimal32Type>().value(row)), ty)?
        }
        DataType::Decimal64(..) => {
            decimal_value(i128::from(array.as_primitive::<adt::Decimal64Type>().value(row)), ty)?
        }
        DataType::Decimal128(..) => decimal_value(array.as_primitive::<adt::Decimal128Type>().value(row), ty)?,
        DataType::Decimal256(_, scale) => {
            let unscaled = array.as_primitive::<adt::Decimal256Type>().value(row).to_string();
            let number: f64 = unscaled
                .parse()
                .map_err(|_| invalid("A decimal in a Parquet file is not a number"))?;
            Value::double(number / 10f64.powi(i32::from(*scale)))
        }
        DataType::List(_) => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let items = list_items(array.as_list::<i32>().value(row), &child)?;
            Value::list(child, items)?
        }
        DataType::LargeList(_) => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let items = list_items(array.as_list::<i64>().value(row), &child)?;
            Value::list(child, items)?
        }
        DataType::FixedSizeList(..) => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let items = list_items(array.as_fixed_size_list().value(row), &child)?;
            Value::list(child, items)?
        }
        DataType::Struct(_) => {
            let parts = array.as_struct();
            let mut fields = Vec::with_capacity(parts.num_columns());
            for (column, (_, child)) in parts.columns().iter().zip(ty.struct_children()) {
                fields.push(array_value(column, row, child)?);
            }
            Value::new(ty.clone(), ValueData::Struct(fields))?
        }
        DataType::Map(..) => {
            let map = array.as_map();
            let entries = map.value(row);
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let mut items = Vec::with_capacity(entries.len());
            for i in 0..entries.len() {
                let mut fields = Vec::with_capacity(2);
                for (column, (_, child)) in entries.columns().iter().zip(entry.struct_children()) {
                    fields.push(array_value(column, i, child)?);
                }
                items.push(Value::new(entry.clone(), ValueData::Struct(fields))?);
            }
            Value::new(ty.clone(), ValueData::List(items))?
        }
        DataType::Dictionary(..) => {
            let plain = arrow::compute::cast(array, ty_arrow_value(array.data_type()))
                .map_err(|e| invalid(format!("Could not read a dictionary in a Parquet file: {e}")))?;
            array_value(&plain, row, ty)?
        }
        other => return Err(unsupported(format!("the type {other}"))),
    })
}

fn ty_arrow_value(ty: &DataType) -> &DataType {
    match ty {
        DataType::Dictionary(_, value) => value,
        other => other,
    }
}

/// How far a scan got.
#[derive(Default)]
pub struct ParquetScanState {
    file: usize,
    reader: Option<ParquetRecordBatchReader>,
    /// for each column of the scan, the column of this file
    mapping: Vec<Option<usize>>,
    file_types: Vec<LogicalType>,
    row: i64,
    virtual_rows: crate::execution::multi_file::VirtualRows,
}

impl std::fmt::Debug for ParquetScanState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParquetScanState").field("file", &self.file).finish()
    }
}

fn open_file(scan: &ParquetScanData, state: &mut ParquetScanState, path: &str) -> Result<()> {
    let builder = open_builder(path)?.with_batch_size(STANDARD_VECTOR_SIZE);
    let int96 = special_columns(&builder);
    let schema = Arc::clone(builder.schema());
    let names: Vec<&String> = schema.fields().iter().map(|f| f.name()).collect();
    let mut mapping = Vec::with_capacity(scan.names.len());
    let mut file_types = Vec::with_capacity(scan.names.len());
    for name in &scan.names {
        let found = names.iter().position(|n| n.eq_ignore_ascii_case(name));
        if found.is_none() && !scan.union_by_name {
            return Err(invalid(format!(
                "Failed to read file \"{path}\": schema mismatch in glob: column \"{name}\" was read from the \
                 original file \"{}\", but could not be found in file \"{path}\".\nCandidate names: {}\nIf you are \
                 trying to read files with different schemas, try setting union_by_name=True",
                scan.files[0],
                names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", "),
            )));
        }
        file_types.push(match found {
            Some(column) => column_type(
                schema.field(column),
                &int96,
                column,
                scan.binary_as_string,
                scan.int96_as,
            )?,
            None => LogicalType::SQLNULL,
        });
        mapping.push(found);
    }
    state.reader = Some(guarded(path, || {
        builder
            .build()
            .map_err(|e| invalid(format!("Could not read \"{path}\": {e}")))
    })?);
    state.mapping = mapping;
    state.file_types = file_types;
    state.row = 0;
    Ok(())
}

fn constant_column(ty: &LogicalType, value: &Value, count: usize) -> Result<Vector> {
    let mut column = Vector::new(ty.clone(), count);
    for row in 0..count {
        column.set_value(row, value)?;
    }
    Ok(column)
}

fn batch_chunk(
    scan: &ParquetScanData,
    state: &mut ParquetScanState,
    batch: &RecordBatch,
    path: &str,
) -> Result<DataChunk> {
    let count = batch.num_rows();
    let mut columns = Vec::with_capacity(scan.types.len() + 2);
    for (index, ty) in scan.types.iter().enumerate() {
        let Some(column) = state.mapping[index] else {
            columns.push(constant_column(ty, &Value::null(ty.clone()), count)?);
            continue;
        };
        let file_type = &state.file_types[index];
        let array = batch.column(column);
        let mut vector = Vector::new(file_type.clone(), count);
        for row in 0..count {
            vector.set_value(row, &array_value(array, row, file_type)?)?;
        }
        if file_type == ty {
            columns.push(vector);
        } else {
            let mut cast = Vector::new(ty.clone(), count);
            cast_vector(&vector, &mut cast, count, CastMode::Strict)?;
            columns.push(cast);
        }
    }
    if scan.file_row_number {
        let mut numbers = Vector::new(LogicalType::BIGINT, count);
        for row in 0..count {
            numbers.set_value(row, &Value::bigint(state.row + row as i64))?;
        }
        columns.push(numbers);
    }
    state.row += count as i64;
    if scan.filename {
        columns.push(constant_column(&LogicalType::VARCHAR, &Value::varchar(path), count)?);
    }
    if !scan.hive.is_empty() {
        let parts = hive_parts(path);
        for (key, ty) in &scan.hive {
            let value = parts
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .and_then(|(_, v)| v.clone());
            let value = value.map_or(Value::null(LogicalType::VARCHAR), Value::varchar);
            let strings = constant_column(&LogicalType::VARCHAR, &value, count)?;
            let mut typed = Vector::new(ty.clone(), count);
            cast_vector(&strings, &mut typed, count, CastMode::Strict)?;
            columns.push(typed);
        }
    }
    Ok(DataChunk::from_columns(columns, count))
}

/// The `read_parquet` table function.
#[derive(Debug)]
pub struct PhysicalParquetScan {
    pub types: Vec<LogicalType>,
    pub scan: Arc<ParquetScanData>,
}

impl PhysicalOperator for PhysicalParquetScan {
    fn name(&self) -> &'static str {
        "READ_PARQUET"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(ParquetScanState::default())
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let state: &mut ParquetScanState = state.downcast_mut().expect("parquet scan state");
        loop {
            let Some(path) = self.scan.files.get(state.file) else {
                return Ok(None);
            };
            if state.reader.is_none() {
                open_file(&self.scan, state, path)?;
            }
            let reader = state.reader.as_mut().expect("just opened");
            match guarded(path, || Ok(reader.next()))? {
                Some(batch) => {
                    let batch = batch.map_err(|e| invalid(format!("Could not read \"{path}\": {e}")))?;
                    if batch.num_rows() > 0 {
                        let chunk = batch_chunk(&self.scan, state, &batch, path)?;
                        return crate::execution::multi_file::append_virtual_columns(
                            chunk,
                            &self.scan.virtual_columns,
                            path,
                            state.file,
                            &mut state.virtual_rows,
                        )
                        .map(Some);
                    }
                }
                None => {
                    state.reader = None;
                    state.file += 1;
                }
            }
        }
    }
}

/// `PARTITION_BY`: files in `key=value` directories.
#[derive(Debug, Clone)]
pub struct Partitioning {
    /// the columns of the rows that name a file
    pub columns: Vec<usize>,
    pub write_columns: bool,
    /// `{i}` and `{uuid}` become a number and a UUID
    pub filename_pattern: String,
    pub extension: String,
}

/// What a COPY TO says of the file.
#[derive(Debug, Clone, Default)]
pub struct ParquetWriteOptions {
    pub writer: crate::execution::parquet_writer::WriterOptions,
    pub partitioning: Option<Partitioning>,
}

impl Partitioning {
    /// The columns that go in a file, of `count`, by their position in the rows.
    pub fn written_columns(&self, count: usize) -> Vec<usize> {
        (0..count)
            .filter(|column| self.write_columns || !self.columns.contains(column))
            .collect()
    }

    /// The directory of a row's partition, under `path`.
    pub fn directory(&self, path: &str, names: &[String], chunk: &DataChunk, row: usize) -> Result<String> {
        let mut directory = path.to_string();
        for &column in &self.columns {
            let value = chunk.get_value(column, row);
            let text = if value.is_null() {
                "__HIVE_DEFAULT_PARTITION__".to_string()
            } else {
                hive_escape(&value.try_to_string()?)
            };
            directory += &format!("/{}={text}", hive_escape(&names[column]));
        }
        Ok(directory)
    }

    /// The rows of `chunk`, grouped by the directory of their partition, in the order they first appear.
    pub fn group_rows(&self, path: &str, names: &[String], chunk: &DataChunk) -> Result<Vec<(String, Vec<usize>)>> {
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in 0..chunk.len() {
            let directory = self.directory(path, names, chunk, row)?;
            match index.get(&directory) {
                Some(&at) => groups[at].1.push(row),
                None => {
                    index.insert(directory.clone(), groups.len());
                    groups.push((directory, vec![row]));
                }
            }
        }
        Ok(groups)
    }

    /// Creates the directory of a partition, and says the path of its file.
    pub fn create_file_path(&self, directory: &str) -> Result<String> {
        std::fs::create_dir_all(directory)
            .map_err(|e| Error::new(ErrorKind::Io, format!("Cannot create directory \"{directory}\": {e}")))?;
        let name = self.filename_pattern.replace("{i}", "0");
        Ok(format!("{directory}/{name}.{}", self.extension))
    }
}

/// `HivePartitioning::Escape`
fn hive_escape(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => result.push(char::from(byte)),
            _ => result += &format!("%{byte:02X}"),
        }
    }
    result
}

/// The codec a `compression` option names, and the level of ZSTD.
pub fn parse_compression(name: &str, level: Option<i32>) -> Result<Codec> {
    Ok(match name.to_lowercase().as_str() {
        "uncompressed" | "none" => Codec::Uncompressed,
        "snappy" => Codec::Snappy,
        "gzip" => Codec::Gzip,
        "zstd" => Codec::Zstd(level.unwrap_or(3)),
        "brotli" => Codec::Brotli,
        "lz4" | "lz4_raw" => Codec::Lz4Raw,
        other => {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("Expected uncompressed, snappy, gzip, zstd, lz4, lz4_raw or brotli, got {other}"),
            ));
        }
    })
}

/// `PhysicalCopyToFile` with FORMAT PARQUET.
pub struct PhysicalCopyToParquet {
    types: Vec<LogicalType>,
    options: ParquetWriteOptions,
    children: Vec<Box<dyn PhysicalOperator>>,
    path: String,
    names: Vec<String>,
    /// by the directory of a partition, or `""` without partitions
    writers: Mutex<HashMap<String, ParquetFileWriter>>,
    rows: AtomicU64,
}

impl std::fmt::Debug for PhysicalCopyToParquet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalCopyToParquet")
            .field("path", &self.path)
            .finish()
    }
}

impl PhysicalCopyToParquet {
    pub fn new(
        types: Vec<LogicalType>,
        path: String,
        names: Vec<String>,
        options: ParquetWriteOptions,
        children: Vec<Box<dyn PhysicalOperator>>,
    ) -> Self {
        Self {
            types,
            options,
            children,
            path,
            names,
            writers: Mutex::new(HashMap::new()),
            rows: AtomicU64::new(0),
        }
    }

    /// The columns that go in a file, by their position in the rows.
    fn written_columns(&self) -> Vec<usize> {
        match &self.options.partitioning {
            Some(partitioning) => partitioning.written_columns(self.names.len()),
            None => (0..self.names.len()).collect(),
        }
    }

    fn create(&self, path: &str) -> Result<ParquetFileWriter> {
        let types = self.children[0].types();
        let columns = self.written_columns();
        let names: Vec<String> = columns.iter().map(|&c| self.names[c].clone()).collect();
        let types: Vec<LogicalType> = columns.iter().map(|&c| types[c].clone()).collect();
        ParquetFileWriter::create(path, &names, &types, self.options.writer.clone())
    }

    /// Writes `rows` of `chunk` to the file of `directory`.
    fn write_rows(&self, directory: &str, chunk: &DataChunk, rows: &[usize]) -> Result<()> {
        let mut writers = self.writers.lock();
        if !writers.contains_key(directory) {
            let path = match &self.options.partitioning {
                Some(partitioning) => partitioning.create_file_path(directory)?,
                None => self.path.clone(),
            };
            let writer = self.create(&path)?;
            writers.insert(directory.to_string(), writer);
        }
        let columns: Vec<Vec<Value>> = self
            .written_columns()
            .into_iter()
            .map(|column| rows.iter().map(|&row| chunk.get_value(column, row)).collect())
            .collect();
        writers.get_mut(directory).expect("just opened").append(columns)
    }
}

impl PhysicalOperator for PhysicalCopyToParquet {
    fn name(&self) -> &'static str {
        "COPY_TO_FILE"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn children(&self) -> &[Box<dyn PhysicalOperator>] {
        &self.children
    }
    fn is_sink(&self) -> bool {
        true
    }
    fn sink(&self, chunk: &DataChunk) -> Result<()> {
        match &self.options.partitioning {
            Some(partitioning) => {
                for (directory, rows) in partitioning.group_rows(&self.path, &self.names, chunk)? {
                    self.write_rows(&directory, chunk, &rows)?;
                }
            }
            None => {
                let rows: Vec<usize> = (0..chunk.len()).collect();
                self.write_rows("", chunk, &rows)?;
            }
        }
        self.rows.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn finalize(&self) -> Result<()> {
        let mut writers = self.writers.lock();
        if writers.is_empty() {
            if self.options.partitioning.is_some() {
                std::fs::create_dir_all(&self.path).map_err(|e| {
                    Error::new(ErrorKind::Io, format!("Cannot create directory \"{}\": {e}", self.path))
                })?;
                return Ok(());
            }
            writers.insert(String::new(), self.create(&self.path)?);
        }
        let mut directories: Vec<String> = writers.keys().cloned().collect();
        directories.sort();
        for directory in directories {
            let writer = writers.remove(&directory).expect("listed");
            writer.finish()?;
        }
        Ok(())
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(false)
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let done = state.downcast_mut::<bool>().expect("copy state");
        if *done {
            return Ok(None);
        }
        *done = true;
        let mut count = Vector::new(LogicalType::BIGINT, 1);
        count.set_value(0, &Value::bigint(self.rows.load(Ordering::Relaxed) as i64))?;
        Ok(Some(DataChunk::from_columns(vec![count], 1)))
    }
}

fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn sql_or_null(text: Option<String>) -> String {
    text.map_or("NULL".to_string(), |t| sql_text(&t))
}

fn sql_number(number: Option<i64>) -> String {
    number.map_or("NULL".to_string(), |n| n.to_string())
}

fn read_metadata(path: &str) -> Result<(ParquetMetaData, u64)> {
    let file = File::open(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let metadata = guarded(path, || {
        ParquetMetaDataReader::new()
            .parse_and_finish(&file)
            .map_err(|e| invalid(format!("Invalid Parquet file \"{path}\": {e}")))
    })?;
    Ok((metadata, size))
}

/// What statistics say, as text: smallest, largest, NULLs and distinct values.
fn statistics_text(stats: &Statistics) -> (Option<String>, Option<String>, Option<i64>, Option<i64>) {
    fn shown<T: ToString>(min: Option<&T>, max: Option<&T>) -> (Option<String>, Option<String>) {
        (min.map(T::to_string), max.map(T::to_string))
    }
    let bytes = |value: Option<&[u8]>| value.map(|b| String::from_utf8_lossy(b).into_owned());
    let (min, max) = match stats {
        Statistics::Boolean(s) => shown(s.min_opt(), s.max_opt()),
        Statistics::Int32(s) => shown(s.min_opt(), s.max_opt()),
        Statistics::Int64(s) => shown(s.min_opt(), s.max_opt()),
        Statistics::Float(s) => shown(s.min_opt(), s.max_opt()),
        Statistics::Double(s) => shown(s.min_opt(), s.max_opt()),
        Statistics::Int96(_) => (None, None),
        Statistics::ByteArray(s) => (
            bytes(s.min_opt().map(|v| v.data())),
            bytes(s.max_opt().map(|v| v.data())),
        ),
        Statistics::FixedLenByteArray(s) => (
            bytes(s.min_opt().map(|v| v.data())),
            bytes(s.max_opt().map(|v| v.data())),
        ),
    };
    (
        min,
        max,
        stats.null_count_opt().map(|n| n as i64),
        stats.distinct_count_opt().map(|n| n as i64),
    )
}

/// `ParquetStatisticsUtils::ConvertValueInternal`, as text: a statistic of a column, by the DuckDB type that the
/// column reads as. `None` where C++ gives NULL: values that don't fit the type, or infinities.
fn statistic_text(column: &parquet::schema::types::ColumnDescriptor, bytes: &[u8]) -> Option<String> {
    use parquet::basic::{ConvertedType, LogicalType as Logical, TimeUnit as Unit, Type as Physical};
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    let i32_of = |b: &[u8]| b.try_into().ok().map(i32::from_le_bytes);
    let i64_of = |b: &[u8]| b.try_into().ok().map(i64::from_le_bytes);
    let decimal = |unscaled: i128| -> Option<String> {
        let (precision, scale) = (column.type_precision(), column.type_scale());
        if !(1..=38).contains(&precision) {
            return None;
        }
        let limit = 10i128.checked_pow(precision as u32)?;
        if unscaled >= limit || unscaled <= -limit {
            return None;
        }
        Value::decimal(unscaled, precision as u8, scale.max(0) as u8)
            .ok()?
            .try_to_string()
            .ok()
    };
    // big-endian two's complement, as decimals in byte arrays are
    let big_endian = |b: &[u8]| -> Option<i128> {
        if b.is_empty() || b.len() > 16 {
            return None;
        }
        let mut value: i128 = if b[0] & 0x80 != 0 { -1 } else { 0 };
        for &byte in b {
            value = (value << 8) | i128::from(byte);
        }
        Some(value)
    };
    let blob = |b: &[u8]| Value::blob(b.to_vec()).try_to_string().ok();
    let logical = column.logical_type_ref();
    let converted = column.converted_type();
    let is_decimal = matches!(logical, Some(Logical::Decimal(_))) || converted == ConvertedType::DECIMAL;
    let shown = |value: Value| value.try_to_string().ok();
    match column.physical_type() {
        Physical::BOOLEAN => Some((bytes.first()? != &0).to_string()),
        Physical::INT32 => {
            let value = i32_of(bytes)?;
            if is_decimal {
                return decimal(i128::from(value));
            }
            match (logical, converted) {
                (Some(Logical::Integer(int)), _) if !int.is_signed => Some((value as u32).to_string()),
                (_, ConvertedType::UINT_8 | ConvertedType::UINT_16 | ConvertedType::UINT_32) => {
                    Some((value as u32).to_string())
                }
                (Some(Logical::Date), _) | (_, ConvertedType::DATE) => shown(Value::date(Date(value))),
                (Some(Logical::Time(time)), _) => {
                    let micros = i64::from(value) * 1000;
                    if !(0..=MICROS_PER_DAY).contains(&micros) && !time.is_adjusted_to_u_t_c {
                        return None;
                    }
                    if time.is_adjusted_to_u_t_c {
                        shown(Value::time_tz(TimeTz::new(Time(micros), 0)))
                    } else {
                        shown(Value::time(Time(micros)))
                    }
                }
                (_, ConvertedType::TIME_MILLIS) => {
                    let micros = i64::from(value) * 1000;
                    (0..=MICROS_PER_DAY)
                        .contains(&micros)
                        .then(|| shown(Value::time(Time(micros))))?
                }
                _ => Some(value.to_string()),
            }
        }
        Physical::INT64 => {
            let value = i64_of(bytes)?;
            if is_decimal {
                return decimal(i128::from(value));
            }
            match (logical, converted) {
                (Some(Logical::Integer(int)), _) if !int.is_signed => Some((value as u64).to_string()),
                (_, ConvertedType::UINT_64) => Some((value as u64).to_string()),
                (Some(Logical::Timestamp(ts)), _) => {
                    let utc = ts.is_adjusted_to_u_t_c;
                    let value = match ts.unit {
                        Unit::MILLIS => Value::timestamp(Timestamp(value.checked_mul(1000)?)),
                        Unit::MICROS => Value::timestamp(Timestamp(value)),
                        Unit::NANOS => {
                            return Value::new(LogicalType::new(LogicalTypeId::TimestampNs), ValueData::Int64(value))
                                .ok()
                                .and_then(shown);
                        }
                    };
                    let value = if utc {
                        value.reinterpret(&LogicalType::new(LogicalTypeId::TimestampTz)).ok()?
                    } else {
                        value
                    };
                    shown(value)
                }
                (_, ConvertedType::TIMESTAMP_MILLIS) => shown(Value::timestamp(Timestamp(value.checked_mul(1000)?))),
                (_, ConvertedType::TIMESTAMP_MICROS) => shown(Value::timestamp(Timestamp(value))),
                (Some(Logical::Time(time)), _) => {
                    let micros = match time.unit {
                        Unit::NANOS => value / 1000,
                        _ => value,
                    };
                    if time.is_adjusted_to_u_t_c {
                        shown(Value::time_tz(TimeTz::new(Time(micros), 0)))
                    } else if (0..=MICROS_PER_DAY).contains(&micros) {
                        shown(Value::time(Time(micros)))
                    } else {
                        None
                    }
                }
                (_, ConvertedType::TIME_MICROS) => (0..=MICROS_PER_DAY)
                    .contains(&value)
                    .then(|| shown(Value::time(Time(value))))?,
                _ => Some(value.to_string()),
            }
        }
        Physical::INT96 => {
            // `ImpalaTimestampToTimestamp`: nanoseconds of the day, then the Julian day
            if bytes.len() != 12 {
                return None;
            }
            let nanos = i64::from_le_bytes(bytes[..8].try_into().ok()?);
            let julian = i64::from(u32::from_le_bytes(bytes[8..].try_into().ok()?));
            const JULIAN_TO_UNIX_EPOCH_DAYS: i64 = 2_440_588;
            let micros = (julian - JULIAN_TO_UNIX_EPOCH_DAYS)
                .checked_mul(MICROS_PER_DAY)?
                .checked_add(nanos / 1000)?;
            shown(Value::timestamp(Timestamp(micros)))
        }
        Physical::FLOAT => {
            let value = f32::from_le_bytes(bytes.try_into().ok()?);
            value.is_finite().then(|| shown(Value::float(value)))?
        }
        Physical::DOUBLE => {
            let value = f64::from_le_bytes(bytes.try_into().ok()?);
            value.is_finite().then(|| shown(Value::double(value)))?
        }
        Physical::BYTE_ARRAY => {
            if is_decimal {
                return decimal(big_endian(bytes)?);
            }
            let text = matches!(logical, Some(Logical::String | Logical::Enum | Logical::Json))
                || matches!(
                    converted,
                    ConvertedType::UTF8 | ConvertedType::ENUM | ConvertedType::JSON
                );
            match std::str::from_utf8(bytes) {
                Ok(text_value) if text => Some(text_value.to_string()),
                _ => blob(bytes),
            }
        }
        Physical::FIXED_LEN_BYTE_ARRAY => {
            if is_decimal {
                return decimal(big_endian(bytes)?);
            }
            match logical {
                Some(Logical::Uuid) if bytes.len() == 16 => {
                    let stored = i128::from_be_bytes(bytes.try_into().ok()?) ^ (1 << 127);
                    shown(Value::uuid(stored))
                }
                Some(Logical::Float16) if bytes.len() == 2 => {
                    let value = half_to_f32(u16::from_le_bytes([bytes[0], bytes[1]]));
                    value.is_finite().then(|| shown(Value::float(value)))?
                }
                _ => blob(bytes),
            }
        }
    }
}

/// `Float16ToFloat32`.
fn half_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let fraction = f32::from(bits & 0x3ff);
    match exponent {
        0 => sign * fraction * 2f32.powi(-24),
        31 if fraction == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + fraction / 1024.0) * 2f32.powi(exponent - 15),
    }
}

/// `parquet_metadata`: a row of SQL per column of a row group.
pub fn metadata_rows(path: &str) -> Result<Vec<String>> {
    let (metadata, _) = read_metadata(path)?;
    let mut rows = Vec::new();
    for (group, row_group) in metadata.row_groups().iter().enumerate() {
        for (column, chunk) in row_group.columns().iter().enumerate() {
            let (_, _, nulls, distinct) = chunk.statistics().map_or((None, None, None, None), statistics_text);
            let descriptor = chunk.column_descr();
            let stat = |bytes: Option<&[u8]>| bytes.and_then(|b| statistic_text(descriptor, b));
            let min = stat(chunk.statistics().and_then(Statistics::min_bytes_opt));
            let max = stat(chunk.statistics().and_then(Statistics::max_bytes_opt));
            let encodings: Vec<String> = chunk.encodings().map(|e| e.to_string()).collect();
            rows.push(format!(
                "({}, {group}, {}, {}, {}, {column}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, NULL::BIGINT, {}, {}, \
                 {}, {}, NULL::MAP(BLOB, BLOB), {}, {}, {}, {}, {})",
                sql_text(path),
                row_group.num_rows(),
                row_group.num_columns(),
                row_group.total_byte_size(),
                chunk.file_offset(),
                chunk.num_values(),
                sql_text(&chunk.column_path().parts().join(", ")),
                sql_text(&chunk.column_type().to_string()),
                sql_or_null(min.clone()),
                sql_or_null(max.clone()),
                sql_number(nulls),
                sql_number(distinct),
                sql_or_null(min),
                sql_or_null(max),
                sql_text(chunk.compression().to_string().split('(').next().unwrap_or_default()),
                sql_text(&encodings.join(", ")),
                sql_number(chunk.dictionary_page_offset()),
                chunk.data_page_offset(),
                chunk.compressed_size(),
                chunk.uncompressed_size(),
                sql_number(chunk.bloom_filter_offset()),
                sql_number(chunk.bloom_filter_length().map(i64::from)),
                chunk.statistics().is_some_and(Statistics::min_is_exact),
                chunk.statistics().is_some_and(Statistics::max_is_exact),
                row_group.compressed_size(),
            ));
        }
    }
    Ok(rows)
}

fn schema_rows(ty: &Type, path: &str, column: &mut i64, rows: &mut Vec<String>) {
    let info = ty.get_basic_info();
    let repetition = info.has_repetition().then(|| info.repetition().to_string());
    let converted = info.converted_type();
    let converted = (converted != parquet::basic::ConvertedType::NONE).then(|| converted.to_string());
    let logical = info.logical_type_ref().map(|t| format!("{t:?}"));
    let id = info.has_id().then(|| i64::from(info.id()));
    let (physical, length, scale, precision, children, id_of_column) = match ty {
        Type::PrimitiveType {
            physical_type,
            type_length,
            scale,
            precision,
            ..
        } => {
            let id = *column;
            *column += 1;
            (
                Some(physical_type.to_string()),
                (*type_length > 0).then(|| type_length.to_string()),
                Some(i64::from(*scale)),
                Some(i64::from(*precision)),
                None,
                Some(id),
            )
        }
        Type::GroupType { fields, .. } => (None, None, None, None, Some(fields.len() as i64), None),
    };
    rows.push(format!(
        "({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, NULL, {})",
        sql_text(path),
        sql_text(info.name()),
        sql_or_null(physical),
        sql_or_null(length),
        sql_or_null(repetition),
        sql_number(children),
        sql_or_null(converted),
        sql_number(scale.filter(|s| *s >= 0)),
        sql_number(precision.filter(|p| *p >= 0)),
        sql_number(id),
        sql_or_null(logical),
        sql_number(id_of_column),
    ));
    if let Type::GroupType { fields, .. } = ty {
        for field in fields {
            schema_rows(field, path, column, rows);
        }
    }
}

/// `parquet_schema`: a row of SQL per element of the schema.
pub fn schema_table_rows(path: &str) -> Result<Vec<String>> {
    let (metadata, _) = read_metadata(path)?;
    let mut rows = Vec::new();
    schema_rows(metadata.file_metadata().schema(), path, &mut 0, &mut rows);
    Ok(rows)
}

/// `parquet_kv_metadata`: a row of SQL per key.
pub fn key_value_rows(path: &str) -> Result<Vec<String>> {
    let (metadata, _) = read_metadata(path)?;
    let blob = |text: &str| {
        let bytes: String = text.bytes().map(|b| format!("\\x{b:02X}")).collect();
        format!("'{bytes}'::BLOB")
    };
    Ok(metadata
        .file_metadata()
        .key_value_metadata()
        .map(|pairs| {
            pairs
                .iter()
                .filter(|pair| pair.key != "ARROW:schema")
                .map(|pair| {
                    format!(
                        "({}, {}, {})",
                        sql_text(path),
                        blob(&pair.key),
                        pair.value.as_deref().map_or("NULL".to_string(), blob)
                    )
                })
                .collect()
        })
        .unwrap_or_default())
}

/// `parquet_file_metadata`: the row of SQL of a file.
pub fn file_metadata_row(path: &str) -> Result<String> {
    let (metadata, size) = read_metadata(path)?;
    let file = metadata.file_metadata();
    Ok(format!(
        "({}, {}, {}, {}, {}, NULL, NULL, {size}::UBIGINT, 0::UBIGINT, NULL::VARCHAR[])",
        sql_text(path),
        sql_or_null(file.created_by().map(str::to_string)),
        file.num_rows(),
        metadata.num_row_groups(),
        file.version(),
    ))
}
