// Ported from duckdb/extension/parquet @ rust-port-baseline
//
//! Reading and writing Parquet: the plan's v1, through the `parquet` crate's Arrow reader and writer.
//! Rows pass as `Value`s between Arrow and vectors, so this is complete rather than fast, and
//! neither filters nor projections are pushed into the file.

use super::csv::hive_parts;
use super::physical::{PhysicalOperator, SourceState};
use crate::function::cast::{CastMode, cast_vector};
use arrow::array::{
    Array, ArrayRef, AsArray, BinaryArray, BooleanArray, Decimal128Array, FixedSizeBinaryBuilder, GenericListArray,
    MapArray, PrimitiveArray, RecordBatch, StringArray, StructArray,
};
use arrow::buffer::{NullBuffer, OffsetBuffer};
use arrow::datatypes::{
    self as adt, ArrowPrimitiveType, DataType, Field, FieldRef, Fields, IntervalMonthDayNano, Schema, TimeUnit,
};
use duckrs_common::types::{Date, Interval, LogicalType, LogicalTypeId, Time, Timestamp};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use parking_lot::Mutex;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::basic::{BrotliLevel, Compression, GzipLevel, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use parquet::file::properties::{WriterProperties, WriterVersion};
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

/// The names and types of a file's columns.
pub fn file_schema(path: &str, binary_as_string: bool) -> Result<(Vec<String>, Vec<LogicalType>)> {
    let builder = open_builder(path)?;
    let schema = builder.schema();
    let names = schema.fields().iter().map(|f| f.name().clone()).collect();
    let types = schema
        .fields()
        .iter()
        .map(|f| logical_type(f, binary_as_string))
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
            Value::time(Time(i64::from(seconds) * 1_000_000))
        }
        DataType::Time32(_) => {
            let millis = array.as_primitive::<adt::Time32MillisecondType>().value(row);
            Value::time(Time(i64::from(millis) * 1_000))
        }
        DataType::Time64(TimeUnit::Nanosecond) => Value::time(Time(
            array.as_primitive::<adt::Time64NanosecondType>().value(row) / 1_000,
        )),
        DataType::Time64(_) => Value::time(Time(array.as_primitive::<adt::Time64MicrosecondType>().value(row))),
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
            Some(column) => logical_type(schema.field(column), scan.binary_as_string)?,
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
#[derive(Debug, Clone)]
pub struct ParquetWriteOptions {
    pub compression: Compression,
    pub row_group_size: usize,
    pub version: WriterVersion,
    pub key_values: Vec<(String, String)>,
    pub data_page_size_limit: Option<usize>,
    pub bloom_filter: bool,
    pub bloom_filter_fpp: Option<f64>,
    pub partitioning: Option<Partitioning>,
}

impl Default for ParquetWriteOptions {
    fn default() -> Self {
        Self {
            compression: Compression::SNAPPY,
            row_group_size: 122_880,
            version: WriterVersion::PARQUET_1_0,
            key_values: Vec::new(),
            data_page_size_limit: None,
            bloom_filter: false,
            bloom_filter_fpp: None,
            partitioning: None,
        }
    }
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

/// The codec a `compression` option names.
pub fn parse_compression(name: &str, level: Option<i32>) -> Result<Compression> {
    let wrong = |e: parquet::errors::ParquetError| invalid(e);
    Ok(match name.to_lowercase().as_str() {
        "uncompressed" | "none" => Compression::UNCOMPRESSED,
        "snappy" => Compression::SNAPPY,
        "gzip" => Compression::GZIP(match level {
            Some(level) => GzipLevel::try_new(level.max(0) as u32).map_err(wrong)?,
            None => GzipLevel::default(),
        }),
        "zstd" => Compression::ZSTD(match level {
            Some(level) => ZstdLevel::try_new(level).map_err(wrong)?,
            None => ZstdLevel::default(),
        }),
        "brotli" => Compression::BROTLI(match level {
            Some(level) => BrotliLevel::try_new(level.max(0) as u32).map_err(wrong)?,
            None => BrotliLevel::default(),
        }),
        "lz4" | "lz4_raw" => Compression::LZ4_RAW,
        other => {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("Expected uncompressed, snappy, gzip, zstd, lz4, lz4_raw or brotli, got {other}"),
            ));
        }
    })
}

/// `ParquetWriter::DuckDBTypeToParquetType`, in Arrow.
fn arrow_field(name: &str, ty: &LogicalType) -> Result<Field> {
    let field = Field::new(name, arrow_type(ty)?, true);
    Ok(if ty.id() == LogicalTypeId::Uuid {
        field.with_metadata([(EXTENSION_KEY.to_string(), UUID_EXTENSION.to_string())])
    } else {
        field
    })
}

fn arrow_type(ty: &LogicalType) -> Result<DataType> {
    Ok(match ty.id() {
        LogicalTypeId::Boolean => DataType::Boolean,
        LogicalTypeId::TinyInt => DataType::Int8,
        LogicalTypeId::SmallInt => DataType::Int16,
        LogicalTypeId::Integer => DataType::Int32,
        LogicalTypeId::BigInt => DataType::Int64,
        LogicalTypeId::UTinyInt => DataType::UInt8,
        LogicalTypeId::USmallInt => DataType::UInt16,
        LogicalTypeId::UInteger => DataType::UInt32,
        LogicalTypeId::UBigInt => DataType::UInt64,
        LogicalTypeId::Float => DataType::Float32,
        LogicalTypeId::Double | LogicalTypeId::HugeInt | LogicalTypeId::UHugeInt => DataType::Float64,
        LogicalTypeId::Varchar | LogicalTypeId::Enum | LogicalTypeId::TimeTz | LogicalTypeId::Bignum => DataType::Utf8,
        LogicalTypeId::Blob | LogicalTypeId::Bit => DataType::Binary,
        LogicalTypeId::Uuid => DataType::FixedSizeBinary(16),
        LogicalTypeId::Date => DataType::Date32,
        LogicalTypeId::Time => DataType::Time64(TimeUnit::Microsecond),
        LogicalTypeId::Timestamp | LogicalTypeId::TimestampSec | LogicalTypeId::TimestampMs => {
            DataType::Timestamp(TimeUnit::Microsecond, None)
        }
        LogicalTypeId::TimestampNs => DataType::Timestamp(TimeUnit::Nanosecond, None),
        LogicalTypeId::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        LogicalTypeId::Interval => DataType::Interval(adt::IntervalUnit::MonthDayNano),
        LogicalTypeId::Decimal => {
            let (width, scale) = ty.decimal_properties().unwrap_or((38, 0));
            DataType::Decimal128(width, scale as i8)
        }
        LogicalTypeId::List | LogicalTypeId::Array => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::INTEGER);
            DataType::List(Arc::new(arrow_field("element", &child)?))
        }
        LogicalTypeId::Struct => {
            let fields: Vec<FieldRef> = ty
                .struct_children()
                .iter()
                .map(|(name, child)| arrow_field(name, child).map(Arc::new))
                .collect::<Result<_>>()?;
            DataType::Struct(Fields::from(fields))
        }
        LogicalTypeId::Map => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::INTEGER);
            let children = entry.struct_children();
            let key = Field::new("key", arrow_type(&children[0].1)?, false);
            let value = arrow_field("value", &children[1].1)?;
            let entries = Field::new("key_value", DataType::Struct(Fields::from(vec![key, value])), false);
            DataType::Map(Arc::new(entries), false)
        }
        LogicalTypeId::SqlNull => DataType::Int32,
        _ => return Err(crate::unported(format!("writing {ty} to a Parquet file"))),
    })
}

fn primitive<T: ArrowPrimitiveType>(
    values: &[&Value],
    data_type: DataType,
    get: impl Fn(&Value) -> Option<T::Native>,
) -> ArrayRef {
    let items = values.iter().map(|v| if v.is_null() { None } else { get(v) });
    Arc::new(PrimitiveArray::<T>::from_iter(items).with_data_type(data_type))
}

/// The values of a nested column, and which of its rows are NULL.
fn list_parts<'a>(values: &[&'a Value]) -> (Vec<i32>, Vec<&'a Value>, NullBuffer) {
    let mut offsets = Vec::with_capacity(values.len() + 1);
    offsets.push(0i32);
    let mut items = Vec::new();
    for value in values {
        if let ValueData::List(children) = value.data() {
            items.extend(children.iter());
        }
        offsets.push(items.len() as i32);
    }
    let nulls = NullBuffer::from_iter(values.iter().map(|v| !v.is_null()));
    (offsets, items, nulls)
}

/// A column of Arrow from its values.
fn build_array(values: &[&Value], ty: &LogicalType) -> Result<ArrayRef> {
    let data_type = arrow_type(ty)?;
    let int = |v: &Value| v.as_i128();
    Ok(match ty.id() {
        LogicalTypeId::Boolean => Arc::new(
            values
                .iter()
                .map(|v| match v.data() {
                    ValueData::Bool(flag) => Some(*flag),
                    _ => None,
                })
                .collect::<BooleanArray>(),
        ),
        LogicalTypeId::TinyInt => primitive::<adt::Int8Type>(values, data_type, |v| int(v).map(|i| i as i8)),
        LogicalTypeId::SmallInt => primitive::<adt::Int16Type>(values, data_type, |v| int(v).map(|i| i as i16)),
        LogicalTypeId::Integer | LogicalTypeId::SqlNull => {
            primitive::<adt::Int32Type>(values, data_type, |v| int(v).map(|i| i as i32))
        }
        LogicalTypeId::BigInt => primitive::<adt::Int64Type>(values, data_type, |v| int(v).map(|i| i as i64)),
        LogicalTypeId::UTinyInt => primitive::<adt::UInt8Type>(values, data_type, |v| int(v).map(|i| i as u8)),
        LogicalTypeId::USmallInt => primitive::<adt::UInt16Type>(values, data_type, |v| int(v).map(|i| i as u16)),
        LogicalTypeId::UInteger => primitive::<adt::UInt32Type>(values, data_type, |v| int(v).map(|i| i as u32)),
        LogicalTypeId::UBigInt => primitive::<adt::UInt64Type>(values, data_type, |v| int(v).map(|i| i as u64)),
        LogicalTypeId::Float => primitive::<adt::Float32Type>(values, data_type, |v| match v.data() {
            ValueData::Float(x) => Some(*x),
            _ => None,
        }),
        LogicalTypeId::Double => primitive::<adt::Float64Type>(values, data_type, |v| match v.data() {
            ValueData::Double(x) => Some(*x),
            _ => None,
        }),
        LogicalTypeId::HugeInt | LogicalTypeId::UHugeInt => {
            primitive::<adt::Float64Type>(values, data_type, |v| match v.data() {
                ValueData::Int128(x) => Some(*x as f64),
                ValueData::UInt128(x) => Some(*x as f64),
                _ => None,
            })
        }
        LogicalTypeId::Varchar | LogicalTypeId::Enum | LogicalTypeId::TimeTz | LogicalTypeId::Bignum => {
            let texts: Vec<Option<String>> = values
                .iter()
                .map(|v| {
                    if v.is_null() {
                        Ok(None)
                    } else {
                        v.try_to_string().map(Some)
                    }
                })
                .collect::<Result<_>>()?;
            Arc::new(texts.into_iter().collect::<StringArray>())
        }
        LogicalTypeId::Blob | LogicalTypeId::Bit => {
            let bytes: Vec<Option<&[u8]>> = values
                .iter()
                .map(|v| match v.data() {
                    ValueData::Bytes(b) => Some(b.as_slice()),
                    _ => None,
                })
                .collect();
            Arc::new(BinaryArray::from(bytes))
        }
        LogicalTypeId::Uuid => {
            let mut builder = FixedSizeBinaryBuilder::with_capacity(values.len(), 16);
            for value in values {
                match value.data() {
                    ValueData::Int128(stored) => builder
                        .append_value(((*stored as u128) ^ (1 << 127)).to_be_bytes())
                        .map_err(invalid)?,
                    _ => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
        LogicalTypeId::Date => primitive::<adt::Date32Type>(values, data_type, |v| int(v).map(|i| i as i32)),
        LogicalTypeId::Time => primitive::<adt::Time64MicrosecondType>(values, data_type, |v| int(v).map(|i| i as i64)),
        LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz => {
            primitive::<adt::TimestampMicrosecondType>(values, data_type, |v| int(v).map(|i| i as i64))
        }
        LogicalTypeId::TimestampSec => {
            primitive::<adt::TimestampMicrosecondType>(values, data_type, |v| int(v).map(|i| i as i64 * 1_000_000))
        }
        LogicalTypeId::TimestampMs => {
            primitive::<adt::TimestampMicrosecondType>(values, data_type, |v| int(v).map(|i| i as i64 * 1_000))
        }
        LogicalTypeId::TimestampNs => {
            primitive::<adt::TimestampNanosecondType>(values, data_type, |v| int(v).map(|i| i as i64))
        }
        LogicalTypeId::Interval => primitive::<adt::IntervalMonthDayNanoType>(values, data_type, |v| match v.data() {
            ValueData::Interval(x) => Some(IntervalMonthDayNano::new(x.months, x.days, x.micros * 1_000)),
            _ => None,
        }),
        LogicalTypeId::Decimal => {
            let items = values.iter().map(|v| if v.is_null() { None } else { v.as_i128() });
            Arc::new(Decimal128Array::from_iter(items).with_data_type(data_type))
        }
        LogicalTypeId::List | LogicalTypeId::Array => {
            let child = ty.child_type().cloned().unwrap_or(LogicalType::INTEGER);
            let (offsets, items, nulls) = list_parts(values);
            let DataType::List(field) = data_type else {
                unreachable!("a list is a list");
            };
            Arc::new(GenericListArray::<i32>::new(
                field,
                OffsetBuffer::new(offsets.into()),
                build_array(&items, &child)?,
                Some(nulls),
            ))
        }
        LogicalTypeId::Struct => {
            let DataType::Struct(fields) = data_type else {
                unreachable!("a struct is a struct");
            };
            let null = Value::sql_null();
            let mut arrays = Vec::with_capacity(fields.len());
            for (index, (_, child)) in ty.struct_children().iter().enumerate() {
                let items: Vec<&Value> = values
                    .iter()
                    .map(|v| match v.data() {
                        ValueData::Struct(parts) => &parts[index],
                        _ => &null,
                    })
                    .collect();
                arrays.push(build_array(&items, child)?);
            }
            let nulls = NullBuffer::from_iter(values.iter().map(|v| !v.is_null()));
            Arc::new(StructArray::new(fields, arrays, Some(nulls)))
        }
        LogicalTypeId::Map => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::INTEGER);
            let (offsets, items, nulls) = list_parts(values);
            let DataType::Map(field, sorted) = data_type else {
                unreachable!("a map is a map");
            };
            let DataType::Struct(fields) = field.data_type().clone() else {
                unreachable!("map entries are a struct");
            };
            let null = Value::sql_null();
            let mut arrays = Vec::with_capacity(2);
            for (index, (_, child)) in entry.struct_children().iter().enumerate() {
                let parts: Vec<&Value> = items
                    .iter()
                    .map(|v| match v.data() {
                        ValueData::Struct(parts) => &parts[index],
                        _ => &null,
                    })
                    .collect();
                arrays.push(build_array(&parts, child)?);
            }
            let entries = StructArray::new(fields, arrays, None);
            Arc::new(MapArray::new(
                field,
                OffsetBuffer::new(offsets.into()),
                entries,
                Some(nulls),
                sorted,
            ))
        }
        _ => return Err(crate::unported(format!("writing {ty} to a Parquet file"))),
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
    writers: Mutex<HashMap<String, ArrowWriter<File>>>,
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

    fn schema(&self) -> Result<Arc<Schema>> {
        let types = self.children[0].types();
        let fields: Vec<Field> = self
            .written_columns()
            .into_iter()
            .map(|column| arrow_field(&self.names[column], &types[column]))
            .collect::<Result<_>>()?;
        Ok(Arc::new(Schema::new(fields)))
    }

    fn failed(&self, error: impl std::fmt::Display) -> Error {
        Error::new(ErrorKind::Io, format!("Could not write \"{}\": {error}", self.path))
    }

    fn create(&self, path: &str) -> Result<ArrowWriter<File>> {
        let file =
            File::create(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
        let key_values: Vec<KeyValue> = self
            .options
            .key_values
            .iter()
            .map(|(key, value)| KeyValue::new(key.clone(), value.clone()))
            .collect();
        let mut properties = WriterProperties::builder()
            .set_compression(self.options.compression)
            .set_max_row_group_row_count(Some(self.options.row_group_size.max(1)))
            .set_writer_version(self.options.version)
            .set_created_by("DuckDB".to_string())
            .set_bloom_filter_enabled(self.options.bloom_filter)
            .set_key_value_metadata((!key_values.is_empty()).then_some(key_values));
        if let Some(limit) = self.options.data_page_size_limit {
            properties = properties.set_data_page_size_limit(limit);
        }
        if let Some(fpp) = self.options.bloom_filter_fpp {
            properties = properties.set_bloom_filter_fpp(fpp);
        }
        ArrowWriter::try_new(file, self.schema()?, Some(properties.build())).map_err(|e| self.failed(e))
    }

    /// Writes `rows` of `chunk` to the file of `directory`.
    fn write_rows(&self, directory: &str, chunk: &DataChunk, rows: &[usize]) -> Result<()> {
        let types = self.children[0].types();
        let mut writers = self.writers.lock();
        if !writers.contains_key(directory) {
            let path = match &self.options.partitioning {
                Some(partitioning) => partitioning.create_file_path(directory)?,
                None => self.path.clone(),
            };
            let writer = self.create(&path)?;
            writers.insert(directory.to_string(), writer);
        }
        let mut arrays = Vec::new();
        for column in self.written_columns() {
            let values: Vec<Value> = rows.iter().map(|&row| chunk.get_value(column, row)).collect();
            let values: Vec<&Value> = values.iter().collect();
            arrays.push(build_array(&values, &types[column])?);
        }
        let batch = RecordBatch::try_new(self.schema()?, arrays).map_err(|e| self.failed(e))?;
        let writer = writers.get_mut(directory).expect("just opened");
        writer.write(&batch).map_err(|e| self.failed(e))
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
        for (_, writer) in writers.drain() {
            writer.close().map_err(|e| self.failed(e))?;
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

/// `parquet_metadata`: a row of SQL per column of a row group.
pub fn metadata_rows(path: &str) -> Result<Vec<String>> {
    let (metadata, _) = read_metadata(path)?;
    let mut rows = Vec::new();
    for (group, row_group) in metadata.row_groups().iter().enumerate() {
        for (column, chunk) in row_group.columns().iter().enumerate() {
            let (min, max, nulls, distinct) = chunk.statistics().map_or((None, None, None, None), statistics_text);
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
