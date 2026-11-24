// Ported from duckdb/extension/parquet @ rust-port-baseline
//
//! Reading and writing Parquet: the plan's v1, through the `parquet` crate's Arrow reader and writer.
//! Rows pass as `Value`s between Arrow and vectors, so this is complete rather than fast, and
//! neither filters nor projections are pushed into the file.

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
#[derive(Debug, Clone)]
pub struct ParquetScanData {
    pub files: Vec<String>,
    /// the columns that come from the files
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub binary_as_string: bool,
    /// `int96_as`
    pub int96_as: Int96As,
    /// `enable_geoparquet_conversion`: the columns of the GeoParquet metadata are GEOMETRY
    pub geoparquet_conversion: bool,
    /// `utf8_validation`
    pub utf8_validation: Utf8Validation,
    /// with COPY ... FROM, the columns of the table the columns of the files are loaded into
    pub table_columns: Vec<String>,
    pub union_by_name: bool,
    pub file_row_number: bool,
    pub filename: bool,
    pub hive: Vec<(String, LogicalType)>,
    /// the virtual columns, after the rest
    pub virtual_columns: Vec<crate::execution::multi_file::VirtualColumn>,
    /// `schema`: the columns by field id or name, in place of those of the first file
    pub schema: Option<Arc<Vec<crate::execution::parquet_schema::SchemaColumn>>>,
    /// by the columns from the files, those the query reads (`column_ids`); the others are NULL
    pub read: Option<Vec<bool>>,
    /// the hive partitions that are also columns of the files, by the position of the column
    pub hive_overrides: Vec<(usize, String)>,
    /// the name the function was called by, which EXPLAIN shows (`read_parquet`, `parquet_scan`)
    pub function_name: String,
    /// the arguments of the call, which tell two calls apart (`LogicalGet::parameters`)
    pub arguments: String,
    /// `table_filters`: conditions on one column each, over the columns of [`Self::output_types`]
    pub filters: Vec<crate::planner::expression::BoundExpr>,
    /// `OptionalFilter`s: implied by a filter above the scan, they only let it skip what none of their rows pass
    pub optional_filters: Vec<crate::planner::expression::BoundExpr>,
    /// by output column, those the operators above read (`projection_ids`), once unused columns are known
    pub projected: Option<Vec<bool>>,
    /// `ExtraOperatorInfo::file_filters`: the filters that left files out, as text
    pub file_filters: String,
    /// the order the binder read the columns in (`column_ids`), which EXPLAIN lists them in
    pub column_order: crate::execution::multi_file::ColumnOrder,
    /// `ExtraOperatorInfo::total_files`: how many files there were before that
    pub total_files: Option<usize>,
    /// the fields of columns pushed into the scan (`IsPushdownExtract`): the output column they are of, and
    /// what gives them; they are output columns after all others
    pub extracts: Vec<(usize, Arc<crate::planner::logical::PushedExtract>)>,
    /// `ParquetReaderProjectionExpressionType::BYTE_LENGTH`: the columns from the files read as the byte length of
    /// their values (`strlen`, `octet_length` pushed into the scan), which are BIGINT in [`Self::types`]
    pub byte_lengths: Vec<usize>,
    /// `projection_ids`: once a projection of just these is omitted, the columns of
    /// [`Self::output_types`] the scan emits, in order
    pub output: Option<Vec<usize>>,
    /// `maximum_files_for_stats`: a scan of at most this many files opens them all at bind, for their statistics
    /// and row counts (0: never)
    pub files_for_stats: usize,
}

impl ParquetScanData {
    /// The output column at `at` as `HivePartitioning::GetKnownColumnValues` knows it: a hive partition or the
    /// file name, which have one value for each file.
    fn known_column(&self, at: usize) -> Option<KnownColumn<'_>> {
        if let Some((_, key)) = self.hive_overrides.iter().find(|(column, _)| *column == at) {
            return Some(KnownColumn::Hive(key, &self.types[at]));
        }
        let mut next = self.names.len() + usize::from(self.file_row_number);
        if self.filename {
            if at == next {
                return Some(KnownColumn::Filename);
            }
            next += 1;
        }
        if let Some((key, ty)) = at.checked_sub(next).and_then(|i| self.hive.get(i)) {
            return Some(KnownColumn::Hive(key, ty));
        }
        next += self.hive.len();
        match at.checked_sub(next).and_then(|i| self.virtual_columns.get(i)) {
            Some(crate::execution::multi_file::VirtualColumn::Filename) => Some(KnownColumn::Filename),
            _ => None,
        }
    }

    /// Whether the output column at `at` has one value for each file, which [`Self::known_value`] gives.
    pub fn is_known_per_file(&self, at: usize) -> bool {
        self.known_column(at).is_some()
    }

    /// The value of the output column at `at` in the file at `path`, if it has one for the whole file.
    pub fn known_value(&self, at: usize, path: &str) -> Option<Value> {
        match self.known_column(at)? {
            KnownColumn::Filename => Some(Value::varchar(path)),
            KnownColumn::Hive(key, ty) => super::csv::hive_value(path, key, ty).ok(),
        }
    }

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
        types.extend(self.extracts.iter().map(|(_, e)| e.expression.return_type.clone()));
        types
    }

    /// The types of the columns the scan emits: [`Self::output_types`], or those of [`Self::output`].
    pub fn emitted_types(&self) -> Vec<LogicalType> {
        let types = self.output_types();
        match &self.output {
            Some(output) => output.iter().map(|&at| types[at].clone()).collect(),
            None => types,
        }
    }
}

/// A column with one value for each file.
enum KnownColumn<'a> {
    Filename,
    /// the key of the partition, and the type of the column
    Hive(&'a str, &'a LogicalType),
}

/// The numbers in `text`, in order.
fn numbers_in(text: &str) -> Vec<u64> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// The codecs of the column chunks of the file at `path`.
fn file_codecs(path: &str) -> Vec<parquet::basic::Compression> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(metadata) = ParquetMetaDataReader::new().parse_and_finish(&file) else {
        return Vec::new();
    };
    metadata
        .row_groups()
        .iter()
        .flat_map(|group| group.columns().iter().map(|column| column.compression()))
        .collect()
}

/// The error C++ gives for the corruption the crate reports as `message`, in the file at `path` (the checks of
/// `ParquetReader`, `ColumnReader` and their decoders); `None` where there is no such error to give.
fn corrupt_file_error(path: &str, message: &str) -> Option<Error> {
    use parquet::basic::Compression as Codec;
    let numbers = numbers_in(message);
    let error = |kind: ErrorKind, text: String| Some(Error::new(kind, text));
    if message.contains("Parquet file too small. Size is") {
        return error(
            ErrorKind::DataCorruption,
            format!("Footer length error in file '{path}'"),
        );
    }
    if message.contains("Actual decompressed size doesn't match the expected one") && numbers.len() >= 2 {
        let codecs = file_codecs(path);
        let name = if codecs.iter().any(|c| matches!(c, Codec::GZIP(_))) {
            "GZIP"
        } else if codecs.iter().any(|c| matches!(c, Codec::BROTLI(_))) {
            "Brotli"
        } else {
            return None;
        };
        return error(
            ErrorKind::DataCorruption,
            format!(
                "Failed to read file \"{path}\": {name} uncompressed data size mismatch (expected {}, actual: {})",
                numbers[1], numbers[0]
            ),
        );
    }
    if message.contains("unexpected end of file") && file_codecs(path).iter().any(|c| matches!(c, Codec::GZIP(_))) {
        return error(
            ErrorKind::Invalid,
            "Failed to decompress GZIP block: incomplete GZIP member footer".to_string(),
        );
    }
    // `ByteBuffer`'s bounds checks
    if message.contains("not enough data to read levels") || message.contains("insufficient values read from column") {
        return error(ErrorKind::Invalid, "Out of buffer".to_string());
    }
    if message.contains("out of sync in read_records") && numbers.len() >= 2 {
        return error(
            ErrorKind::InvalidInput,
            format!(
                "Mismatch in parquet read, expected {} rows, got {}",
                numbers[numbers.len() - 2],
                numbers[numbers.len() - 1]
            ),
        );
    }
    if (message.contains("Invalid DECIMAL precision:") || message.contains("Invalid DECIMAL scale:"))
        && let Some((precision, scale)) = invalid_decimal(path)
    {
        return error(
            ErrorKind::Io,
            format!("Invalid DECIMAL precision {precision} and scale {scale}"),
        );
    }
    if message.contains("Invalid DECIMAL: scale") && numbers.len() >= 2 {
        return error(
            ErrorKind::Io,
            format!("Invalid DECIMAL precision {} and scale {}", numbers[1], numbers[0]),
        );
    }
    if message.contains("Invalid or corrupted RLE bit width") && numbers.len() >= 2 {
        return error(
            ErrorKind::Io,
            format!(
                "The width ({}) of the bitpacked data exceeds the maximum width ({}) for the target type, the file \
                 might be corrupted.",
                numbers[0], numbers[1]
            ),
        );
    }
    if message.contains("cannot have zero miniblocks per block") {
        return error(
            ErrorKind::InvalidInput,
            "Parquet file has invalid number of miniblocks per block for DELTA_BINARY_PACKED".to_string(),
        );
    }
    if message.contains("encountered non UTF-8 data") {
        return error(
            ErrorKind::InvalidInput,
            format!("Invalid string encoding found in Parquet file \"{path}\": value is not valid UTF8!"),
        );
    }
    if let Some(at) = message.find(" cannot annotate field ") {
        // e.g. `INT_64 cannot annotate field 'val' because it is not a INT64 field`
        let converted = message[..at].rsplit(' ').next().unwrap_or_default().replace('_', "");
        let physical = message
            .rsplit("not a ")
            .next()
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_default();
        if converted == "INTERVAL" {
            return error(
                ErrorKind::DataCorruption,
                "INTERVAL converted type can only be set for value of Type::FIXED_LEN_BYTE_ARRAY of length 12"
                    .to_string(),
            );
        }
        return error(
            ErrorKind::Io,
            format!("{converted} converted type can only be set for value of Type::{physical}"),
        );
    }
    if message.contains("Required field encodings is missing") {
        return error(
            ErrorKind::InvalidInput,
            format!("Failed to read file \"{path}\": column chunk is missing its metadata"),
        );
    }
    if let Some(rest) = message.split("Invalid FIXED_LEN_BYTE_ARRAY length: ").nth(1) {
        // e.g. `-1 for field 'u'`
        let length = rest.split(' ').next().unwrap_or_default();
        let field = rest.split('\'').nth(1).unwrap_or_default();
        return error(
            ErrorKind::Io,
            format!("Invalid negative type_length {length} for column \"{field}\""),
        );
    }
    if message.contains("column start and length should not be negative") {
        return error(
            ErrorKind::DataCorruption,
            format!("Failed to read file \"{path}\": metadata is corrupt. Column chunk has a negative offset or size"),
        );
    }
    None
}

/// An error of the crate reading the file at `path`: what C++ says of the same corruption, if it is one.
fn read_error(path: &str, prefix: &str, error: impl std::fmt::Display) -> Error {
    let message = error.to_string();
    corrupt_file_error(path, &message).unwrap_or_else(|| invalid(format!("{prefix} \"{path}\": {message}")))
}

/// Runs the crate on a file that may be corrupt: it panics on some, which must stay one query's error.
fn guarded<T>(path: &str, read: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).unwrap_or_else(|panic| {
        let reason = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("the file is corrupt");
        Err(corrupt_file_error(path, reason).unwrap_or_else(|| {
            invalid(format!(
                "Invalid Parquet file \"{path}\": {reason}. Parquet file is likely corrupted"
            ))
        }))
    })
}

fn open_builder(path: &str) -> Result<ParquetRecordBatchReaderBuilder<File>> {
    let file = File::open(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
    let builder = guarded(path, || {
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| read_error(path, "Invalid Parquet file", e))
    })?;
    check_schema_depth(builder.parquet_schema().root_schema(), path)?;
    check_variant_groups(builder.parquet_schema().root_schema(), path)?;
    Ok(builder)
}

/// `MAX_PARQUET_SCHEMA_DEPTH`: deeper schemas would overflow the stack while being read.
const MAX_PARQUET_SCHEMA_DEPTH: usize = 500;

/// The depth check of `ParquetReader::ParseSchemaRecursive`, the root at depth 0, walked without
/// recursion so that the check itself can't overflow the stack.
fn check_schema_depth(root: &parquet::schema::types::Type, path: &str) -> Result<()> {
    let mut pending = vec![(root, 0usize)];
    while let Some((ty, depth)) = pending.pop() {
        if depth > MAX_PARQUET_SCHEMA_DEPTH {
            return Err(invalid(format!(
                "Malformed Parquet schema in file \"{path}\": the schema is nested more than \
                 {MAX_PARQUET_SCHEMA_DEPTH} levels deep"
            )));
        }
        if ty.is_group() {
            pending.extend(ty.get_fields().iter().map(|field| (field.as_ref(), depth + 1)));
        }
    }
    Ok(())
}

/// A FIXED_LEN_BYTE_ARRAY(12) annotated INTERVAL.
fn is_interval(ty: &parquet::schema::types::Type) -> bool {
    ty.is_primitive()
        && ty.get_physical_type() == parquet::basic::Type::FIXED_LEN_BYTE_ARRAY
        && ty.get_basic_info().converted_type() == parquet::basic::ConvertedType::INTERVAL
}

fn has_interval(ty: &parquet::schema::types::Type) -> bool {
    if ty.is_group() {
        ty.get_fields().iter().any(|field| has_interval(field))
    } else {
        is_interval(ty)
    }
}

/// [`open_builder`] with some annotations taken out of the schema the crate converts, so that it reads the bytes:
/// - with `strings`, those of STRING columns, which it would check are UTF-8, for a repairing `utf8_validation`
/// - those of INTERVALs, whose months its intervals (DayTime) leave out
fn open_builder_as_bytes(path: &str, strings: bool) -> Result<ParquetRecordBatchReaderBuilder<File>> {
    use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
    use parquet::basic::{ConvertedType, LogicalType as Logical};
    use parquet::file::metadata::{FileMetaData, ParquetMetaData};
    use parquet::schema::types::{SchemaDescriptor, Type as SchemaType, TypePtr};
    fn strip(ty: &TypePtr, strings: bool) -> parquet::errors::Result<TypePtr> {
        let info = ty.get_basic_info();
        if ty.is_group() {
            let fields = ty
                .get_fields()
                .iter()
                .map(|field| strip(field, strings))
                .collect::<parquet::errors::Result<Vec<_>>>()?;
            let mut builder = SchemaType::group_type_builder(info.name())
                .with_fields(fields)
                .with_logical_type(info.logical_type_ref().cloned())
                .with_converted_type(info.converted_type());
            if info.has_repetition() {
                builder = builder.with_repetition(info.repetition());
            }
            if info.has_id() {
                builder = builder.with_id(Some(info.id()));
            }
            return Ok(Arc::new(builder.build()?));
        }
        let is_string = strings
            && ty.get_physical_type() == parquet::basic::Type::BYTE_ARRAY
            && (matches!(info.logical_type_ref(), Some(Logical::String))
                || info.converted_type() == ConvertedType::UTF8);
        if !is_string && !is_interval(ty) {
            return Ok(Arc::clone(ty));
        }
        let mut builder =
            SchemaType::primitive_type_builder(info.name(), ty.get_physical_type()).with_repetition(info.repetition());
        if let SchemaType::PrimitiveType { type_length, .. } = ty.as_ref() {
            builder = builder.with_length(*type_length);
        }
        if info.has_id() {
            builder = builder.with_id(Some(info.id()));
        }
        Ok(Arc::new(builder.build()?))
    }
    let builder = open_builder(path)?;
    let metadata = builder.metadata();
    let file = metadata.file_metadata();
    let root = guarded(path, || {
        strip(&file.schema_descr().root_schema_ptr(), strings).map_err(|e| read_error(path, "Invalid Parquet file", e))
    })?;
    let file_metadata = FileMetaData::new(
        file.version(),
        file.num_rows(),
        file.created_by().map(str::to_string),
        file.key_value_metadata().cloned(),
        Arc::new(SchemaDescriptor::new(root)),
        file.column_orders().cloned(),
    );
    let stripped = ParquetMetaData::new(file_metadata, metadata.row_groups().to_vec());
    let options = ArrowReaderOptions::new().with_skip_arrow_metadata(true);
    let reader_metadata = ArrowReaderMetadata::try_new(Arc::new(stripped), options)
        .map_err(|e| read_error(path, "Invalid Parquet file", e))?;
    let file = File::open(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
    Ok(ParquetRecordBatchReaderBuilder::new_with_metadata(
        file,
        reader_metadata,
    ))
}

/// The types the crate is told to read some columns as (its schema hints):
/// - INT96 timestamps as microseconds where `int96_as` makes TIMESTAMPs of them, as
///   `ImpalaTimestampToMicroseconds` does: the crate's nanoseconds overflow before 1677 and after 2262
/// - INTERVALs with their months, which the crate's default (DayTime) leaves out
fn with_schema_hints(
    path: &str,
    builder: ParquetRecordBatchReaderBuilder<File>,
    special: &[Special],
    int96_as: Int96As,
) -> Result<ParquetRecordBatchReaderBuilder<File>> {
    use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
    fn hint(ty: &DataType) -> Option<DataType> {
        match ty {
            DataType::Interval(adt::IntervalUnit::DayTime) => Some(DataType::Interval(adt::IntervalUnit::MonthDayNano)),
            DataType::List(child) => Some(DataType::List(Arc::new(hint_field(child)?))),
            DataType::LargeList(child) => Some(DataType::LargeList(Arc::new(hint_field(child)?))),
            DataType::Struct(fields) => {
                let mut changed = false;
                let fields: Vec<Field> = fields
                    .iter()
                    .map(|f| match hint_field(f) {
                        Some(f) => {
                            changed = true;
                            f
                        }
                        None => f.as_ref().clone(),
                    })
                    .collect();
                changed.then(|| DataType::Struct(fields.into()))
            }
            DataType::Map(entries, sorted) => Some(DataType::Map(Arc::new(hint_field(entries)?), *sorted)),
            _ => None,
        }
    }
    fn hint_field(field: &Field) -> Option<Field> {
        hint(field.data_type()).map(|ty| field.clone().with_data_type(ty))
    }
    let schema = builder.schema();
    let mut changed = false;
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| {
            if special.get(index) == Some(&Special::Int96) && int96_as == Int96As::Timestamp {
                changed = true;
                return field
                    .as_ref()
                    .clone()
                    .with_data_type(DataType::Timestamp(TimeUnit::Microsecond, None));
            }
            if special.get(index) == Some(&Special::Variant) {
                return field.as_ref().clone();
            }
            match hint_field(field) {
                Some(hinted) => {
                    changed = true;
                    hinted
                }
                None => field.as_ref().clone(),
            }
        })
        .collect();
    if !changed {
        return Ok(builder);
    }
    let hinted = Arc::new(adt::Schema::new_with_metadata(fields, schema.metadata().clone()));
    let options = ArrowReaderOptions::new().with_schema(hinted);
    let metadata = ArrowReaderMetadata::try_new(Arc::clone(builder.metadata()), options)
        .map_err(|e| read_error(path, "Invalid Parquet file", e))?;
    let file = File::open(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
    Ok(ParquetRecordBatchReaderBuilder::new_with_metadata(file, metadata))
}

/// `VariantColumnReader`: a VARIANT group starts with its `metadata` and `value`.
fn check_variant_groups(ty: &parquet::schema::types::Type, path: &str) -> Result<()> {
    if ty.is_primitive() {
        return Ok(());
    }
    let fields = ty.get_fields();
    if matches!(
        ty.get_basic_info().logical_type_ref(),
        Some(parquet::basic::LogicalType::Variant(_))
    ) {
        let name = |i: usize| fields.get(i).map(|f| f.name());
        let ok = matches!(
            (name(0), name(1)),
            (Some("metadata"), Some("value")) | (Some("value"), Some("metadata"))
        );
        if !ok {
            return Err(Error::new(
                ErrorKind::DataCorruption,
                format!(
                    "Failed to read file \"{path}\": the Variant column must have 'metadata' and 'value' as the first two columns"
                ),
            ));
        }
    }
    fields.iter().try_for_each(|field| check_variant_groups(field, path))
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
        DataType::Time64(TimeUnit::Nanosecond) => LogicalType::new(LogicalTypeId::TimeNs),
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
            deduplicate_names(fields.iter().map(|f| f.name()))
                .into_iter()
                .zip(fields.iter())
                .map(|(name, f)| Ok((name, logical_type(f, binary_as_string)?)))
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

/// `CheckParquetTime`: a TIME (not one in UTC, which wraps) is within a day, 24:00:00 included.
fn check_time(raw: i64, day: i64, ty: &LogicalType) -> Result<()> {
    if ty.id() == LogicalTypeId::TimeTz || (0..=day).contains(&raw) {
        return Ok(());
    }
    Err(invalid(format!(
        "Invalid TIME value {raw} in Parquet file - TIME values must be within a day"
    )))
}

/// `ParquetWrapTime`: a TIMETZ of any number of microseconds, as the time of day it falls on.
fn time_tz_value(micros: i64) -> Value {
    Value::time_tz(TimeTz::new(Time(micros.rem_euclid(86_400_000_000)), 0))
}

/// `StringColumnReader::Utf8ValidationOption`: what reading invalid UTF-8 into a VARCHAR does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Utf8Validation {
    /// an error
    #[default]
    Strict,
    /// each invalid byte becomes `?` (`Utf8Proc::MakeValid`)
    Replace,
    /// invalid bytes are left out (`Utf8Proc::RemoveInvalid`)
    Ignore,
}

impl Utf8Validation {
    /// `GetUtf8ValidationOption`
    pub fn parse(text: &str) -> Result<Self> {
        match text.to_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "replace" => Ok(Self::Replace),
            "ignore" => Ok(Self::Ignore),
            _ => Err(Error::new(
                ErrorKind::Binder,
                format!(
                    "utf8_validation option \"{text}\" not recognized, must be one of 'strict', 'replace', 'ignore'"
                ),
            )),
        }
    }

    /// `RepairsInvalidUTF8`: the values no longer lie within the statistics of the stored bytes.
    pub fn repairs(self) -> bool {
        self != Self::Strict
    }
}

thread_local! {
    /// The `utf8_validation` of the scan converting values on this thread.
    static UTF8_VALIDATION: std::cell::Cell<Utf8Validation> = const { std::cell::Cell::new(Utf8Validation::Strict) };
}

/// Runs `f` with the values it converts repaired as `mode` says.
fn with_utf8_validation<T>(mode: Utf8Validation, f: impl FnOnce() -> T) -> T {
    let previous = UTF8_VALIDATION.with(|cell| cell.replace(mode));
    let result = f();
    UTF8_VALIDATION.with(|cell| cell.set(previous));
    result
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

/// A TIME, or with `ty` TIMETZ, a TIME in UTC wrapped into the day (`ParquetIntToTimeTZ`).
fn time_value(micros: i64, ty: &LogicalType) -> Value {
    if ty.id() == LogicalTypeId::TimeTz {
        time_tz_value(micros)
    } else {
        Value::time(Time(micros))
    }
}

/// What the crate leaves out of a top-level column's type.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Special {
    None,
    /// INT96 (Impala timestamps), which the crate reads as nanoseconds
    Int96,
    /// a TIME adjusted to UTC, which DuckDB reads as TIMETZ
    TimeUtc,
    /// a group annotated as VARIANT
    Variant,
    /// annotated UNKNOWN: a column of NULLs, whatever its physical type
    Null,
    /// WKB, annotated GEOMETRY or GEOGRAPHY or named in the GeoParquet metadata, with its CRS (empty for none)
    Geometry(String),
}

// Ported from duckdb/extension/parquet/parquet_reader.cpp (IsGeometryType) @ rust-port-baseline
fn special_columns(builder: &ParquetRecordBatchReaderBuilder<File>, geoparquet: bool) -> Result<Vec<Special>> {
    use parquet::basic::LogicalType as L;
    // `GeoParquetFileMetadata::TryRead`
    let geo_columns = match builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kvs| kvs.iter().find(|kv| kv.key == "geo"))
        .and_then(|kv| kv.value.as_deref())
        .filter(|_| geoparquet)
    {
        Some(geo) => crate::execution::parquet_writer::geoparquet::try_read(geo)?,
        None => None,
    };
    Ok(builder
        .parquet_schema()
        .root_schema()
        .get_fields()
        .iter()
        .map(|field| {
            if !field.is_primitive() {
                return match field.get_basic_info().logical_type_ref() {
                    Some(L::Variant(_)) => Special::Variant,
                    _ => Special::None,
                };
            }
            if field.get_physical_type() == parquet::basic::Type::BYTE_ARRAY {
                match field.get_basic_info().logical_type_ref() {
                    Some(L::Geometry(geometry)) => {
                        return Special::Geometry(geometry.crs.clone().unwrap_or_default());
                    }
                    Some(L::Geography(geography)) => {
                        return Special::Geometry(geography.crs.clone().unwrap_or_default());
                    }
                    _ => {}
                }
                // a GeoParquet column is a leaf at the root, named in the metadata
                if let Some((_, projjson)) = geo_columns
                    .as_ref()
                    .and_then(|columns| columns.iter().find(|(name, _)| name == field.name()))
                {
                    return Special::Geometry(projjson.clone());
                }
            }
            if field.get_physical_type() == parquet::basic::Type::INT96 {
                return Special::Int96;
            }
            match field.get_basic_info().logical_type_ref() {
                Some(L::Time(time)) if time.is_adjusted_to_u_t_c => Special::TimeUtc,
                Some(L::Unknown) => Special::Null,
                _ => Special::None,
            }
        })
        .collect())
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
    if special.get(index) == Some(&Special::Variant) {
        return Ok(LogicalType::new(LogicalTypeId::Variant));
    }
    if special.get(index) == Some(&Special::Null) {
        return Ok(LogicalType::SQLNULL);
    }
    if let Some(Special::Geometry(crs)) = special.get(index) {
        // `CoordinateReferenceSystem::TryIdentify`: a CRS nothing can tell is none
        let crs = duckrs_common::geometry_crs::try_identify(crs).ok().flatten();
        return Ok(LogicalType::geometry(crs));
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

/// `ParseSchemaRecursive`: names that are the same but for case get `_1`, `_2`, ... appended.
pub fn deduplicate_names<'a>(names: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    names
        .into_iter()
        .map(|name| {
            let mut name = name.clone();
            while let Some(count) = counts.get_mut(&name.to_lowercase()) {
                *count += 1;
                name = format!("{name}_{count}");
            }
            counts.insert(name.to_lowercase(), 0);
            name
        })
        .collect()
}

/// The names and types of a file's columns.
pub fn file_schema(
    path: &str,
    binary_as_string: bool,
    int96_as: Int96As,
    geoparquet: bool,
) -> Result<(Vec<String>, Vec<LogicalType>)> {
    let builder = open_builder(path)?;
    let int96 = special_columns(&builder, geoparquet)?;
    let schema = builder.schema();
    let names = deduplicate_names(schema.fields().iter().map(|f| f.name()));
    let types = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| column_type(f, &int96, i, binary_as_string, int96_as))
        .collect::<Result<_>>()?;
    Ok((names, types))
}

/// That the columns of `schema` map onto those of the file at `path` (`MultiFileColumnMapper`).
pub fn check_schema(
    path: &str,
    binary_as_string: bool,
    int96_as: Int96As,
    geoparquet: bool,
    columns: &[crate::execution::parquet_schema::SchemaColumn],
) -> Result<()> {
    let builder = open_builder(path)?;
    let int96 = special_columns(&builder, geoparquet)?;
    let schema = builder.schema();
    let fields: Vec<&Field> = schema.fields().iter().map(|f| f.as_ref()).collect();
    for column in columns {
        if let Some(index) = crate::execution::parquet_schema::find_field(&fields, &column.key) {
            let file_type = column_type(schema.field(index), &int96, index, binary_as_string, int96_as)?;
            crate::execution::parquet_schema::build_transform(column, schema.field(index), &file_type)?;
        }
    }
    Ok(())
}

fn utf8(bytes: &[u8]) -> Result<Value> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(Value::varchar(text));
    }
    match UTF8_VALIDATION.with(std::cell::Cell::get) {
        Utf8Validation::Strict => Err(invalid(
            "Invalid string encoding found in Parquet file: value is not valid UTF8!",
        )),
        Utf8Validation::Replace => Ok(Value::varchar(super::csv::make_valid(bytes, '?'))),
        Utf8Validation::Ignore => Ok(Value::varchar(super::csv::remove_invalid(bytes))),
    }
}

fn binary_value(bytes: &[u8], ty: &LogicalType) -> Result<Value> {
    match ty.id() {
        LogicalTypeId::Varchar => utf8(bytes),
        // months, days and milliseconds (`ParquetIntervalUtils`)
        LogicalTypeId::Interval if bytes.len() == 12 => {
            let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
            Ok(Value::interval(Interval {
                months: word(0) as i32,
                days: word(4) as i32,
                micros: i64::from(word(8)) * 1000,
            }))
        }
        LogicalTypeId::Uuid => {
            let raw = <[u8; 16]>::try_from(bytes).map_err(|_| invalid("A UUID in a Parquet file is not 16 bytes"))?;
            Ok(Value::uuid((u128::from_be_bytes(raw) ^ (1 << 127)) as i128))
        }
        // `ST_GeomFromWKB`: little endian ISO WKB, or the error that says why it isn't WKB
        LogicalTypeId::Geometry => Ok(Value::new(
            ty.clone(),
            ValueData::Bytes(duckrs_common::geometry::from_wkb(bytes)?),
        )?),
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
    if ty.id() == LogicalTypeId::TimestampNs
        || (ty.id() == LogicalTypeId::TimestampTzNs && *unit == TimeUnit::Nanosecond)
    {
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
/// The DuckDB type of a field of a file, read as is.
pub(super) fn field_type(field: &Field) -> Result<LogicalType> {
    logical_type(field, false)
}

/// The value of `row` of `array`, as a `ty`.
pub(super) fn field_value(array: &dyn Array, row: usize, ty: &LogicalType) -> Result<Value> {
    array_value(array, row, ty)
}

fn array_value(array: &dyn Array, row: usize, ty: &LogicalType) -> Result<Value> {
    if array.is_null(row) || ty.id() == LogicalTypeId::SqlNull {
        return Ok(Value::null(ty.clone()));
    }
    if ty.id() == LogicalTypeId::Variant
        && let Some(group) = array.as_struct_opt()
    {
        return super::parquet_variant::read_row(group, row);
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
            check_time(i64::from(seconds), 86_400, ty)?;
            time_value(i64::from(seconds) * 1_000_000, ty)
        }
        DataType::Time32(_) => {
            let millis = array.as_primitive::<adt::Time32MillisecondType>().value(row);
            check_time(i64::from(millis), 86_400_000, ty)?;
            time_value(i64::from(millis) * 1_000, ty)
        }
        DataType::Time64(TimeUnit::Nanosecond) => {
            let nanos = array.as_primitive::<adt::Time64NanosecondType>().value(row);
            if ty.id() == LogicalTypeId::TimeNs {
                check_time(nanos, 86_400_000_000_000, ty)?;
                return Value::new(ty.clone(), ValueData::Int64(nanos));
            }
            check_time(nanos, 86_400_000_000_000, ty)?;
            time_value(nanos.div_euclid(1_000), ty)
        }
        DataType::Time64(_) => {
            let micros = array.as_primitive::<adt::Time64MicrosecondType>().value(row);
            check_time(micros, 86_400_000_000, ty)?;
            time_value(micros, ty)
        }
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
    /// with `schema`: how the values of each mapped column become those of the result
    transforms: Vec<Option<crate::execution::parquet_schema::Transform>>,
    row: i64,
    virtual_rows: crate::execution::multi_file::VirtualRows,
}

impl std::fmt::Debug for ParquetScanState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParquetScanState").field("file", &self.file).finish()
    }
}

/// The row groups of the file whose statistics do not rule out every row of them, and how many there are; `None`
/// where all of them are read (`ParquetReader::PrepareRowGroupBuffer`).
fn row_groups_to_scan(
    scan: &ParquetScanData,
    filter: Option<&crate::planner::expression::BoundExpr>,
    metadata: &ParquetMetaData,
    path: &str,
) -> Option<Vec<usize>> {
    let filter = filter?;
    // the rows are numbered as they are read
    let mut numbers = Vec::new();
    if scan.file_row_number {
        numbers.push(scan.names.len());
    }
    let first_virtual = scan.output_types().len() - scan.virtual_columns.len();
    for (at, column) in scan.virtual_columns.iter().enumerate() {
        if *column == crate::execution::multi_file::VirtualColumn::FileRowNumber {
            numbers.push(first_virtual + at);
        }
    }
    fn reads(expr: &crate::planner::expression::BoundExpr, columns: &[usize]) -> bool {
        matches!(expr.kind, crate::planner::expression::BoundExprKind::Reference(at) if columns.contains(&at))
            || expr.children().into_iter().any(|child| reads(child, columns))
    }
    let filtered = reads(filter, &numbers);
    let projected = |at: &usize| {
        scan.projected
            .as_ref()
            .is_none_or(|p| p.get(*at).copied().unwrap_or(true))
    };
    if filtered || numbers.iter().any(projected) {
        return None;
    }
    let leaves: Vec<Option<usize>> = (0..scan.names.len())
        .map(|column| statistics_leaf(scan, metadata, column, path))
        .collect();
    let element_leaves: Vec<Option<(usize, LogicalType)>> = (0..scan.names.len())
        .map(|column| element_leaf(scan, metadata, column, path))
        .collect();
    let normalized = normalized_interval_blooms(metadata);
    let mut file = None;
    let selected = metadata
        .row_groups()
        .iter()
        .enumerate()
        .filter(|(index, group)| {
            // `ColumnReader::Stats` of a VARIANT column in the row group, which the paths of it filtered on have
            // bounds by; each worked out once
            let variant_stats = std::cell::RefCell::new(HashMap::new());
            let variant = |column: usize| -> Option<crate::catalog::ColumnStatistics> {
                let is_variant = scan
                    .types
                    .get(column)
                    .is_some_and(|ty| ty.id() == LogicalTypeId::Variant)
                    && column < scan.names.len()
                    && scan.schema.is_none()
                    && !scan
                        .hive_overrides
                        .iter()
                        .any(|(_, key)| key.eq_ignore_ascii_case(&scan.names[column]));
                if !is_variant {
                    return None;
                }
                variant_stats
                    .borrow_mut()
                    .entry(column)
                    .or_insert_with(|| variant_stats::variant_statistics(metadata, &scan.names[column], Some(*index)))
                    .clone()
            };
            let zone = |at: usize| -> Option<crate::catalog::ColumnZone> {
                if let Some((leaf, element)) = element_leaves.get(at).and_then(Option::as_ref) {
                    // `TransformColumnStatistics` of a LIST or MAP: unknown validity, the elements' statistics
                    let elements = group_zone(metadata, group, *leaf, element)?;
                    return (!elements.unknown).then(|| crate::catalog::ColumnZone {
                        has_null: true,
                        elements: Some(Box::new(elements)),
                        ..Default::default()
                    });
                }
                if let Some(leaf) = leaves.get(at) {
                    let zone = group_zone(metadata, group, (*leaf)?, &scan.types[at])?;
                    return (!zone.unknown).then_some(zone);
                }
                let value = scan.known_value(at, path)?;
                Some(crate::catalog::ColumnZone {
                    has_null: value.is_null(),
                    min: (!value.is_null()).then(|| value.clone()),
                    max: (!value.is_null()).then_some(value),
                    unknown: false,
                    ..Default::default()
                })
            };
            !crate::execution::zone_map::never_true_with_variants(&zone, &variant, filter)
                && !bloom_rules_out(
                    scan,
                    &leaves,
                    &element_leaves,
                    group,
                    filter,
                    &mut file,
                    path,
                    normalized,
                )
        })
        .map(|(at, _)| at)
        .collect();
    Some(selected)
}

/// Whether the bloom filters of the row group rule out every row `expr` is true for: an equality of a column and
/// a constant, or conjunctions of those (`ParquetReader::CheckBloomFilters`).
#[allow(clippy::too_many_arguments)]
fn bloom_rules_out(
    scan: &ParquetScanData,
    leaves: &[Option<usize>],
    elements: &[Option<(usize, LogicalType)>],
    group: &parquet::file::metadata::RowGroupMetaData,
    expr: &crate::planner::expression::BoundExpr,
    file: &mut Option<File>,
    path: &str,
    normalized: bool,
) -> bool {
    use crate::planner::expression::BoundExprKind as E;
    match &expr.kind {
        E::Conjunction { and: true, children } => {
            if children
                .iter()
                .any(|child| bloom_rules_out(scan, leaves, elements, group, child, file, path, normalized))
            {
                return true;
            }
            // `x >= c AND x <= c` is `x = c` (what `FilterCombiner` makes of it)
            use duckrs_parser::ast::ComparisonOp as C;
            fn bound(expr: &crate::planner::expression::BoundExpr, wanted: C) -> Option<(usize, Value)> {
                match &expr.kind {
                    E::Comparison { op, left, right } if *op == wanted => match (&left.kind, &right.kind) {
                        (E::Reference(column), E::Constant(value)) => Some((*column, value.clone())),
                        _ => None,
                    },
                    _ => None,
                }
            }
            children
                .iter()
                .filter_map(|c| bound(c, C::GreaterThanOrEqual))
                .any(|(column, low)| {
                    let matched = children
                        .iter()
                        .filter_map(|c| bound(c, C::LessThanOrEqual))
                        .any(|(other, high)| other == column && high == low);
                    let ty = scan.types.get(column).cloned().unwrap_or(LogicalType::SQLNULL);
                    let equal = crate::planner::expression::BoundExpr::new(
                        E::Comparison {
                            op: C::Equal,
                            left: Box::new(crate::planner::expression::BoundExpr::new(E::Reference(column), ty)),
                            right: Box::new(crate::planner::expression::BoundExpr::constant(low)),
                        },
                        LogicalType::BOOLEAN,
                    );
                    matched && bloom_rules_out(scan, leaves, elements, group, &equal, file, path, normalized)
                })
        }
        E::Conjunction { and: false, children } => children
            .iter()
            .all(|child| bloom_rules_out(scan, leaves, elements, group, child, file, path, normalized)),
        // `x BETWEEN c AND c` is `x = c`
        E::Between { input, lower, upper } => match (&input.kind, &lower.kind, &upper.kind) {
            (E::Reference(_), E::Constant(low), E::Constant(high)) if low == high => {
                let equal = crate::planner::expression::BoundExpr::new(
                    E::Comparison {
                        op: duckrs_parser::ast::ComparisonOp::Equal,
                        left: input.clone(),
                        right: lower.clone(),
                    },
                    LogicalType::BOOLEAN,
                );
                bloom_rules_out(scan, leaves, elements, group, &equal, file, path, normalized)
            }
            _ => false,
        },
        // NULL is never in a bloom filter, and `IS NOT DISTINCT FROM` a value that isn't NULL is `=` it
        E::Comparison {
            op: duckrs_parser::ast::ComparisonOp::Equal | duckrs_parser::ast::ComparisonOp::NotDistinctFrom,
            left,
            right,
        } => {
            let (target, value) = match (&left.kind, &right.kind) {
                (_, E::Constant(value)) => (left.as_ref(), value),
                (E::Constant(value), _) => (right.as_ref(), value),
                _ => return false,
            };
            // `TryGetNestedBloomFilterLeaf`: a column, an element of a LIST column or a value of a MAP column
            let (leaf, ty) = match &target.kind {
                E::Reference(column) => match leaves.get(*column) {
                    Some(Some(leaf)) => (*leaf, &scan.types[*column]),
                    _ => return false,
                },
                E::Function { function, children, .. } if children.len() == 2 => {
                    let E::Reference(column) = children[0].kind else {
                        return false;
                    };
                    let extract = match scan.types[column].id() {
                        LogicalTypeId::List => matches!(function.name.as_str(), "list_extract" | "array_extract"),
                        LogicalTypeId::Map => matches!(function.name.as_str(), "map_extract_value" | "array_extract"),
                        _ => false,
                    };
                    match elements.get(column) {
                        Some(Some((leaf, element))) if extract => (*leaf, element),
                        _ => return false,
                    }
                }
                _ => return false,
            };
            let leaf = &leaf;
            let Some(value) = try_cast_value(value, ty).filter(|v| !v.is_null()) else {
                return false;
            };
            if file.is_none() {
                *file = File::open(path).ok();
            }
            let Some(file) = file.as_mut() else {
                return false;
            };
            bloom_excludes(file, group.column(*leaf), ty, &value, normalized).unwrap_or(false)
        }
        _ => false,
    }
}

/// `ParquetColumnSchema::ValidateColumnMetadata` with `validate_row_count`, for the top-level columns
/// `reads` says the scan reads: a column outside lists has one value per row of its row group.
fn validate_column_metadata(path: &str, metadata: &ParquetMetaData, reads: impl Fn(usize) -> bool) -> Result<()> {
    let descr = metadata.file_metadata().schema_descr();
    for group in metadata.row_groups() {
        for (leaf, chunk) in group.columns().iter().enumerate() {
            let column = descr.column(leaf);
            if !reads(descr.get_column_root_idx(leaf)) {
                continue;
            }
            let values = chunk.num_values();
            if values < 0 {
                return Err(Error::new(
                    ErrorKind::DataCorruption,
                    format!(
                        "Failed to read file \"{path}\": metadata is corrupt. Column has invalid number of values \
                         ({values})"
                    ),
                ));
            }
            let top_level = column.path().parts().len() == 1 && column.max_rep_level() == 0;
            if top_level && values != group.num_rows() {
                return Err(Error::new(
                    ErrorKind::DataCorruption,
                    format!(
                        "Failed to read file \"{path}\": metadata is corrupt. Column has {values} values but row \
                         group has {} rows",
                        group.num_rows()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn open_file(
    scan: &ParquetScanData,
    filter: Option<&crate::planner::expression::BoundExpr>,
    state: &mut ParquetScanState,
    path: &str,
) -> Result<u64> {
    let typed = open_builder(path)?;
    let strip = scan.utf8_validation.repairs() || has_interval(typed.parquet_schema().root_schema());
    let builder = if strip {
        open_builder_as_bytes(path, scan.utf8_validation.repairs())?
    } else {
        open_builder(path)?
    };
    let int96 = special_columns(&builder, scan.geoparquet_conversion)?;
    let builder = with_schema_hints(path, builder, &int96, scan.int96_as)?.with_batch_size(STANDARD_VECTOR_SIZE);
    // the types of the columns are those of the file, whatever the reader is told to read strings as
    let schema = Arc::clone(typed.schema());
    let names = deduplicate_names(schema.fields().iter().map(|f| f.name()));
    let mut mapping = Vec::with_capacity(scan.names.len());
    let mut file_types = Vec::with_capacity(scan.names.len());
    let mut transforms = Vec::with_capacity(scan.names.len());
    if let Some(columns) = &scan.schema {
        let fields: Vec<&Field> = schema.fields().iter().map(|f| f.as_ref()).collect();
        for column in columns.iter() {
            let found = crate::execution::parquet_schema::find_field(&fields, &column.key);
            let Some(index) = found else {
                mapping.push(None);
                file_types.push(LogicalType::SQLNULL);
                transforms.push(None);
                continue;
            };
            let file_type = column_type(schema.field(index), &int96, index, scan.binary_as_string, scan.int96_as)?;
            transforms.push(Some(crate::execution::parquet_schema::build_transform(
                column,
                schema.field(index),
                &file_type,
            )?));
            file_types.push(file_type);
            mapping.push(Some(index));
        }
    }
    for name in scan.names.iter().filter(|_| scan.schema.is_none()) {
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
        transforms.push(None);
    }
    validate_column_metadata(path, builder.metadata(), |root| mapping.contains(&Some(root)))?;
    // only the columns the query reads are decoded; a batch holds them in the order of the file
    let mut read_roots: Option<Vec<usize>> = None;
    let builder = match &scan.read {
        Some(read) => {
            let mut roots: Vec<usize> = mapping
                .iter()
                .zip(read)
                .filter(|(_, read)| **read)
                .filter_map(|(column, _)| *column)
                .collect();
            if roots.is_empty() {
                roots.push(0);
            }
            roots.sort_unstable();
            roots.dedup();
            for (column, read) in mapping.iter_mut().zip(read) {
                *column = match column {
                    Some(index) if *read => roots.iter().position(|root| root == index),
                    // nothing reads it: NULL, whatever its type
                    _ => None,
                };
            }
            read_roots = Some(roots.clone());
            let mask = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), roots);
            builder.with_projection(mask)
        }
        None => builder,
    };
    let selected = row_groups_to_scan(scan, filter, builder.metadata(), path);
    // `operator.bytes_scanned`: the column chunks of the row groups read, as they are in the file
    let metadata = Arc::clone(builder.metadata());
    let descr = metadata.file_metadata().schema_descr();
    let groups: Vec<usize> = selected.clone().unwrap_or_else(|| (0..metadata.num_row_groups()).collect());
    let bytes = groups
        .iter()
        .map(|&group| {
            let group = metadata.row_group(group);
            (0..group.num_columns())
                .filter(|&column| read_roots.as_ref().is_none_or(|roots| roots.contains(&descr.get_column_root_idx(column))))
                .map(|column| group.column(column).compressed_size().max(0) as u64)
                .sum::<u64>()
        })
        .sum();
    let builder = match selected {
        Some(selected) => builder.with_row_groups(selected),
        None => builder,
    };
    state.reader = Some(guarded(path, || {
        builder.build().map_err(|e| read_error(path, "Could not read", e))
    })?);
    state.mapping = mapping;
    state.file_types = file_types;
    state.transforms = transforms;
    state.row = 0;
    Ok(bytes)
}

/// A value cast to `ty`, as a cast of its column would.
pub fn cast_value(value: &Value, ty: &LogicalType) -> Result<Value> {
    let mut source = Vector::new(value.logical_type().clone(), 1);
    source.set_value(0, value)?;
    let mut result = Vector::new(ty.clone(), 1);
    cast_vector(&source, &mut result, 1, CastMode::Strict)?;
    Ok(result.get_value(0))
}

/// A value cast to `ty`, or `None` if it doesn't cast.
pub fn try_cast_value(value: &Value, ty: &LogicalType) -> Option<Value> {
    let result = cast_value(value, ty).ok()?;
    (value.is_null() || !result.is_null()).then_some(result)
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
        // a hive partition that is a column of the file as well: the path gives its values (below)
        if scan.hive_overrides.iter().any(|(column, _)| *column == index) {
            columns.push(constant_column(ty, &Value::null(ty.clone()), count)?);
            continue;
        }
        let Some(column) = state.mapping[index] else {
            let default = match &scan.schema {
                Some(schema) => schema[index].default.clone(),
                None => Value::null(ty.clone()),
            };
            columns.push(constant_column(ty, &default, count)?);
            continue;
        };
        let file_type = &state.file_types[index];
        let array = batch.column(column);
        if let Some(transform) = &state.transforms[index]
            && !matches!(transform, crate::execution::parquet_schema::Transform::Direct)
        {
            let mut vector = Vector::new(ty.clone(), count);
            for row in 0..count {
                let value = array_value(array, row, file_type)?;
                let value = crate::execution::parquet_schema::apply(transform, value, ty, &cast_value)?;
                vector.set_value(row, &value)?;
            }
            columns.push(vector);
            continue;
        }
        let vector = match array.as_binary_opt::<i32>() {
            Some(wkb) if file_type.id() == LogicalTypeId::Geometry => geometry_vector(wkb, file_type, count)?,
            _ => {
                let mut vector = Vector::new(file_type.clone(), count);
                for row in 0..count {
                    vector.set_value(row, &array_value(array, row, file_type)?)?;
                }
                vector
            }
        };
        let mut vector = vector;
        if scan.byte_lengths.contains(&index) {
            let mut lengths = Vector::new(LogicalType::BIGINT, count);
            for row in 0..count {
                let length = match vector.get_value(row).data() {
                    ValueData::Bytes(bytes) => Value::bigint(bytes.len() as i64),
                    _ => Value::null(LogicalType::BIGINT),
                };
                lengths.set_value(row, &length)?;
            }
            columns.push(lengths);
            continue;
        }
        // a VARIANT the file has shredded into a `typed_value` reads as a SHREDDED_VECTOR
        if file_type.id() == LogicalTypeId::Variant
            && array.as_struct_opt().is_some_and(|group| group.column_by_name("typed_value").is_some())
        {
            vector.set_shredded();
        }
        if file_type == ty {
            columns.push(vector);
        } else if file_type.id() == LogicalTypeId::Varchar && ty.id() == LogicalTypeId::Blob {
            // text read as BLOB keeps its bytes, which a cast would read as escapes
            let mut blobs = Vector::new(ty.clone(), count);
            for row in 0..count {
                let value = vector.get_value(row);
                let blob = if value.is_null() {
                    Value::null(ty.clone())
                } else {
                    Value::blob(value.try_to_string()?.into_bytes())
                };
                blobs.set_value(row, &blob)?;
            }
            columns.push(blobs);
        } else {
            let mut cast = Vector::new(ty.clone(), count);
            cast_vector(&vector, &mut cast, count, CastMode::Strict)
                .map_err(|error| cast_error(scan, index, path, file_type, ty, error))?;
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
    for (key, ty) in &scan.hive {
        let value = super::csv::hive_value(path, key, ty)?;
        columns.push(constant_column(ty, &value, count)?);
    }
    // the hive partitions that are columns of the files as well take their place
    for (column, key) in &scan.hive_overrides {
        let value = super::csv::hive_value(path, key, &scan.types[*column])?;
        columns[*column] = constant_column(&scan.types[*column], &value, count)?;
    }
    Ok(DataChunk::from_columns(columns, count))
}

// Ported from duckdb/extension/parquet/parquet_geometry.cpp (GeometryColumnReader) @ rust-port-baseline
/// `GeometryColumnReader`: the BLOBs of a GEOMETRY column through `ST_GeomFromWKB`, into a vector of `ty`.
fn geometry_vector(wkb: &arrow::array::BinaryArray, ty: &LogicalType, count: usize) -> Result<Vector> {
    let mut vector = Vector::new(ty.clone(), count);
    for row in 0..count {
        if wkb.is_null(row) {
            vector.set_null(row, true);
        } else {
            vector.set_str(row, &duckrs_common::geometry::from_wkb(wkb.value(row))?);
        }
    }
    Ok(vector)
}

/// `GetExtendedMultiFileError`: a value of a file that does not cast to the type the scan reads.
fn cast_error(
    scan: &ParquetScanData,
    index: usize,
    path: &str,
    from: &LogicalType,
    to: &LogicalType,
    error: Error,
) -> Error {
    if error.kind == ErrorKind::Internal {
        return error;
    }
    let name = &scan.names[index];
    let extended = if scan.table_columns.is_empty() {
        format!(
            "In file \"{path}\" the column \"{name}\" has type {from}, but we are trying to read it as type {to}.\nThis \
             can happen when reading multiple Parquet files. The schema information is taken from the first Parquet file \
             by default. Possible solutions:\n* Enable the union_by_name=True option to combine the schema of all \
             Parquet files (https://duckdb.org/docs/current/data/multiple_files/combining_schemas)\n* Use a COPY \
             statement to automatically derive types from an existing table."
        )
    } else {
        let target = scan
            .table_columns
            .get(index)
            .map(|column| format!("\"{column}\" "))
            .unwrap_or_default();
        format!(
            "In file \"{path}\" the column \"{name}\" has type {from}, but we are trying to load it into column \
             {target}with type {to}.\nThis means the Parquet schema does not match the schema of the table.\nPossible \
             solutions:\n* Insert by name instead of by position using \"INSERT INTO tbl BY NAME SELECT * FROM \
             read_parquet(...)\"\n* Manually specify which columns to insert using \"INSERT INTO tbl SELECT ... FROM \
             read_parquet(...)\""
        )
    };
    Error::new(
        ErrorKind::Conversion,
        format!(
            "Error while reading file \"{path}\": failed to cast column \"{name}\" from type {from} to {to}: : {}\n\n{extended}",
            error.message
        ),
    )
}

/// The `read_parquet` table function.
#[derive(Debug)]
pub struct PhysicalParquetScan {
    pub types: Vec<LogicalType>,
    pub scan: Arc<ParquetScanData>,
    /// the conjunction of the scan's filters, which each row passes
    pub filter: Option<crate::planner::expression::BoundExpr>,
    /// the conjunction of those and the optional filters, which only skip row groups
    pub prune: Option<crate::planner::expression::BoundExpr>,
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
    fn dynamic_filters(&self) -> Option<String> {
        crate::execution::physical::dynamic_filters_of(self.filter.as_ref())
    }
    fn planned_filters(&self) -> Vec<String> {
        crate::execution::physical::top_n_filters_of(self.filter.as_ref())
    }
    fn reports_bytes_scanned(&self) -> bool {
        true
    }
    fn row_groups_scanned(&self) -> Option<(usize, usize)> {
        self.prune.as_ref()?;
        let (mut scanned, mut total) = (0, 0);
        for path in &self.scan.files {
            let (metadata, _) = read_metadata(path).ok()?;
            let groups = metadata.num_row_groups();
            total += groups;
            scanned +=
                row_groups_to_scan(&self.scan, self.prune.as_ref(), &metadata, path).map_or(groups, |g| g.len());
        }
        (total > 0).then_some((scanned, total))
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let state: &mut ParquetScanState = state.downcast_mut().expect("parquet scan state");
        loop {
            let Some(path) = self.scan.files.get(state.file) else {
                return Ok(None);
            };
            if state.reader.is_none() {
                let bytes = open_file(&self.scan, self.prune.as_ref(), state, path)?;
                if let Some(profiler) = crate::execution::profiler::current() {
                    profiler.add_bytes_scanned(self, bytes);
                }
            }
            let reader = state.reader.as_mut().expect("just opened");
            match guarded(path, || Ok(reader.next()))? {
                Some(batch) => {
                    let batch = batch.map_err(|e| read_error(path, "Could not read", e))?;
                    if batch.num_rows() > 0 {
                        let chunk = with_utf8_validation(self.scan.utf8_validation, || {
                            batch_chunk(&self.scan, state, &batch, path)
                        })?;
                        let mut chunk = crate::execution::multi_file::append_virtual_columns(
                            chunk,
                            &self.scan.virtual_columns,
                            path,
                            state.file,
                            &mut state.virtual_rows,
                        )?;
                        if let Some(filter) = &self.filter {
                            let (sel, matched) = crate::execution::expression_executor::select(filter, &chunk)?;
                            if matched == 0 {
                                continue;
                            }
                            if matched != chunk.len() {
                                chunk.slice(&sel, matched);
                            }
                        }
                        // the pushed fields, of the columns they are of
                        if !self.scan.extracts.is_empty() {
                            let mut columns: Vec<_> = (0..chunk.column_count()).map(|c| chunk.column(c).clone()).collect();
                            for (base, extract) in &self.scan.extracts {
                                let input = DataChunk::from_columns(vec![chunk.column(*base).clone()], chunk.len());
                                columns.push(crate::execution::expression_executor::execute(&extract.expression, &input)?);
                            }
                            chunk = DataChunk::from_columns(columns, chunk.len());
                        }
                        if let Some(output) = &self.scan.output {
                            let columns = output.iter().map(|&at| chunk.column(at).clone()).collect();
                            chunk = DataChunk::from_columns(columns, chunk.len());
                        }
                        return Ok(Some(chunk));
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
    /// `HIVE_FILE_PATTERN`: files in `key=value` directories; without it, every file in the target directory
    pub hive_file_pattern: bool,
    /// `PARTITION_PATH`: the directory of a partition, over the values of its columns in order
    pub path_expression: Option<crate::planner::expression::BoundExpr>,
    /// with `PARTITION_PATH`: the partition of each directory, and the directory of each partition
    pub claims: Arc<Mutex<HashMap<String, String>>>,
    /// `partitioned_write_max_open_files`: more open files close the ones opened first
    pub max_open_files: usize,
    /// `partitioned_write_flush_threshold`: the rows a partition buffers before they go to its file
    pub flush_threshold: usize,
    /// `{i}` and `{uuid}` become a number and a UUID
    pub filename_pattern: String,
    pub extension: String,
    /// `ORDER_BY`: each run of buffered rows is sorted by these, over the rows' columns, and written to a file
    pub order: Vec<(
        crate::planner::expression::BoundExpr,
        duckrs_common::sort_key::OrderModifiers,
    )>,
}

/// What a COPY TO says of the file.
#[derive(Debug, Clone, Default)]
pub struct ParquetWriteOptions {
    pub writer: crate::execution::parquet_writer::WriterOptions,
    pub partitioning: Option<Partitioning>,
    pub rotation: Option<Rotation>,
    pub output: CopyOutputOptions,
    /// by column, those the statistics say have no NULLs
    pub not_null_columns: Vec<bool>,
}

impl Partitioning {
    /// `CopyToSortedRuns`: the rows of `pieces` in the order of `ORDER_BY`.
    pub fn sorted(&self, pieces: Vec<DataChunk>) -> Result<Vec<DataChunk>> {
        let Some(first) = pieces.first() else {
            return Ok(pieces);
        };
        if self.order.is_empty() {
            return Ok(pieces);
        }
        let types = first.types();
        let mut keyed: Vec<(Vec<u8>, usize, usize)> = Vec::new();
        for (index, piece) in pieces.iter().enumerate() {
            let vectors = self
                .order
                .iter()
                .map(|(expression, _)| crate::execution::expression_executor::execute(expression, piece))
                .collect::<Result<Vec<_>>>()?;
            let columns: Vec<_> = vectors.iter().zip(&self.order).map(|(v, (_, m))| (v, *m)).collect();
            let keys = duckrs_common::sort_key::create_sort_keys(&columns, piece.len())?;
            keyed.extend(keys.into_iter().enumerate().map(|(row, key)| (key, index, row)));
        }
        keyed.sort();
        let mut sorted: Vec<DataChunk> = Vec::new();
        let mut row_values = Vec::with_capacity(types.len());
        for (_, index, row) in keyed {
            if sorted.last().is_none_or(|chunk| chunk.len() == chunk.capacity()) {
                sorted.push(DataChunk::new(&types));
            }
            row_values.clear();
            row_values.extend((0..types.len()).map(|column| pieces[index].get_value(column, row)));
            sorted.last_mut().expect("just pushed").append_row(&row_values)?;
        }
        Ok(sorted)
    }

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
                hive_escape_value(&value.try_to_string()?)
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
        let Some(expression) = &self.path_expression else {
            return Ok(groups);
        };
        groups
            .into_iter()
            .map(|(hive, rows)| {
                Ok((
                    self.evaluate_path(expression, path, names, chunk, rows[0], &hive)?,
                    rows,
                ))
            })
            .collect()
    }

    /// `PartitionValuesToString`: e.g. `year=2024, month=NULL`.
    fn values_text(&self, names: &[String], chunk: &DataChunk, row: usize) -> String {
        self.columns
            .iter()
            .map(|&column| {
                let value = chunk.get_value(column, row);
                let text = if value.is_null() {
                    "NULL".to_string()
                } else {
                    value.try_to_string().unwrap_or_default()
                };
                format!("{}={text}", names[column])
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `BuildDirectory` with `EvaluatePartitionPath`: the directory PARTITION_PATH gives the partition of `row`.
    fn evaluate_path(
        &self,
        expression: &crate::planner::expression::BoundExpr,
        path: &str,
        names: &[String],
        chunk: &DataChunk,
        row: usize,
        hive: &str,
    ) -> Result<String> {
        let invalid = |message: String| Error::new(ErrorKind::InvalidInput, message);
        let columns = self
            .columns
            .iter()
            .map(|&column| {
                let value = chunk.get_value(column, row);
                let mut vector = Vector::new(chunk.column(column).logical_type().clone(), 1);
                vector.set_value(0, &value)?;
                Ok(vector)
            })
            .collect::<Result<Vec<_>>>()?;
        let values = DataChunk::from_columns(columns, 1);
        let result = crate::execution::expression_executor::execute(expression, &values)?.get_value(0);
        if result.is_null() {
            return Err(invalid(format!(
                "PARTITION_PATH evaluated to NULL for partition {}",
                self.values_text(names, chunk, row)
            )));
        }
        let relative = result.try_to_string()?;
        if relative.starts_with('/') || relative.starts_with('\\') || relative.contains("://") {
            return Err(invalid(format!(
                "PARTITION_PATH must be relative to the COPY target, but got \"{relative}\""
            )));
        }
        let mut components = Vec::new();
        for component in relative.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                return Err(invalid(format!(
                    "PARTITION_PATH cannot contain \"..\", but got \"{relative}\""
                )));
            }
            components.push(component);
        }
        if components.is_empty() {
            return Err(invalid(format!(
                "PARTITION_PATH evaluated to an empty path for partition {}",
                self.values_text(names, chunk, row)
            )));
        }
        let relative = components.join("/");
        let mut claims = self.claims.lock();
        // `ClaimDirectory`: by the hive directory of the partition, and by the directory it gets
        let partition_key = format!("partition:{hive}");
        let directory_key = format!("directory:{relative}");
        if claims.get(&partition_key) != Some(&relative) {
            if let Some(owner) = claims.get(&directory_key) {
                return Err(invalid(format!(
                    "PARTITION_PATH puts partitions ({owner}) and ({}) in the same directory \"{relative}\"",
                    self.values_text(names, chunk, row)
                )));
            }
            claims.insert(directory_key, self.values_text(names, chunk, row));
            claims.insert(partition_key, relative.clone());
        }
        Ok(format!("{path}/{relative}"))
    }

    /// Creates the directory of a partition, and says the path of its file.
    pub fn create_file_path(&self, directory: &str) -> Result<String> {
        self.create_file_path_at(directory, 0)
    }

    /// The path of file number `offset` of a partition's directory, which it creates.
    pub fn create_file_path_at(&self, directory: &str, offset: usize) -> Result<String> {
        std::fs::create_dir_all(directory)
            .map_err(|e| Error::new(ErrorKind::Io, format!("Cannot create directory \"{directory}\": {e}")))?;
        Ok(file_name(directory, &self.filename_pattern, &self.extension, offset))
    }
}

/// `RETURN_FILES`, `RETURN_STATS` and `WRITE_EMPTY_FILE`.
#[derive(Debug, Clone)]
pub struct CopyOutputOptions {
    pub return_files: bool,
    pub return_stats: bool,
    pub write_empty_file: bool,
}

impl Default for CopyOutputOptions {
    fn default() -> Self {
        Self {
            return_files: false,
            return_stats: false,
            write_empty_file: true,
        }
    }
}

/// The files written, and the partition values of the files being written, by path.
type WrittenFiles = (Vec<WrittenFile>, HashMap<String, Vec<(String, String)>>);

/// A file a COPY TO wrote with `RETURN_STATS`, and the values of its partition.
#[derive(Debug, Clone)]
pub struct WrittenFile {
    pub path: String,
    pub stats: crate::execution::parquet_writer::WrittenFileStats,
    pub partition_keys: Option<Vec<(String, String)>>,
}

/// `MAP(VARCHAR, value)` of `entries`.
fn map_value(value_type: LogicalType, entries: Vec<(String, Value)>) -> Result<Value> {
    let items = entries
        .into_iter()
        .map(|(key, value)| {
            Value::struct_value(vec![
                ("key".to_string(), Value::varchar(key)),
                ("value".to_string(), value),
            ])
        })
        .collect();
    Value::new(
        LogicalType::map(LogicalType::VARCHAR, value_type),
        ValueData::List(items),
    )
}

fn to_variant(value: &Value) -> Result<Value> {
    let mut source = Vector::new(value.logical_type().clone(), 1);
    source.set_value(0, value)?;
    let mut result = Vector::new(LogicalType::new(LogicalTypeId::Variant), 1);
    crate::function::cast::cast_vector(&source, &mut result, 1, crate::function::cast::CastMode::Strict)?;
    Ok(result.get_value(0))
}

impl CopyOutputOptions {
    /// `WRITTEN_FILE_STATISTICS`: the names of what a COPY with RETURN_STATS returns.
    pub fn stats_names() -> Vec<String> {
        [
            "filename",
            "count",
            "file_size_bytes",
            "footer_size_bytes",
            "column_statistics",
            "partition_keys",
            "extra_info",
        ]
        .map(String::from)
        .to_vec()
    }

    fn stats_types() -> Vec<LogicalType> {
        let stats_map = LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR);
        vec![
            LogicalType::VARCHAR,
            LogicalType::BIGINT,
            LogicalType::BIGINT,
            LogicalType::BIGINT,
            LogicalType::map(LogicalType::VARCHAR, stats_map.clone()),
            stats_map,
            LogicalType::map(LogicalType::VARCHAR, LogicalType::new(LogicalTypeId::Variant)),
        ]
    }

    /// `ReturnStatistics`: a row for each file.
    pub fn stats_result(&self, files: &[WrittenFile]) -> Result<DataChunk> {
        let types = Self::stats_types();
        let mut columns: Vec<Vector> = types
            .iter()
            .map(|ty| Vector::new(ty.clone(), files.len().max(1)))
            .collect();
        for (row, file) in files.iter().enumerate() {
            let stats = &file.stats;
            let column_statistics = stats
                .column_statistics
                .iter()
                .map(|(name, column)| {
                    let entries = column
                        .iter()
                        .map(|(key, value)| Ok((key.clone(), Value::varchar(value.try_to_string()?))))
                        .collect::<Result<Vec<_>>>()?;
                    Ok((name.clone(), map_value(LogicalType::VARCHAR, entries)?))
                })
                .collect::<Result<Vec<_>>>()?;
            let partition_keys = match &file.partition_keys {
                Some(keys) => map_value(
                    LogicalType::VARCHAR,
                    keys.iter()
                        .map(|(k, v)| (k.clone(), Value::varchar(v.as_str())))
                        .collect(),
                )?,
                None => Value::null(types[5].clone()),
            };
            let extra_info = map_value(
                LogicalType::new(LogicalTypeId::Variant),
                vec![(
                    "row_group_count".to_string(),
                    to_variant(&Value::ubigint(stats.row_group_count))?,
                )],
            )?;
            let values = [
                Value::varchar(file.path.as_str()),
                Value::bigint(stats.row_count as i64),
                Value::bigint(stats.file_size_bytes as i64),
                Value::bigint(stats.footer_size_bytes as i64),
                map_value(
                    LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR),
                    column_statistics,
                )?,
                partition_keys,
                extra_info,
            ];
            for (column, value) in columns.iter_mut().zip(values.iter()) {
                column.set_value(row, value)?;
            }
        }
        Ok(DataChunk::from_columns(columns, files.len()))
    }

    /// The types of what the COPY returns.
    pub fn types(&self) -> Vec<LogicalType> {
        if self.return_stats {
            return Self::stats_types();
        }
        if self.return_files {
            vec![LogicalType::BIGINT, LogicalType::list(LogicalType::VARCHAR)]
        } else {
            vec![LogicalType::BIGINT]
        }
    }

    /// The row a COPY returns: the rows, and with RETURN_FILES the files.
    pub fn result(&self, rows: u64, files: &[String]) -> Result<DataChunk> {
        let mut columns = vec![Vector::new(LogicalType::BIGINT, 1)];
        columns[0].set_value(0, &Value::bigint(rows as i64))?;
        if self.return_files {
            let mut list = Vector::new(LogicalType::list(LogicalType::VARCHAR), 1);
            let mut sorted = files.to_vec();
            sorted.sort();
            let items = sorted.iter().map(|f| Value::varchar(f.as_str())).collect();
            list.set_value(0, &Value::list(LogicalType::VARCHAR, items)?)?;
            columns.push(list);
        }
        Ok(DataChunk::from_columns(columns, 1))
    }
}

/// When a COPY TO starts a new file (`FILE_SIZE_BYTES`, `ROW_GROUPS_PER_FILE`), or writes one per thread
/// (`PER_THREAD_OUTPUT`): into the directory of the path.
#[derive(Debug, Clone)]
pub struct Rotation {
    pub file_size_bytes: Option<usize>,
    pub row_groups_per_file: Option<usize>,
    pub per_thread: bool,
    pub filename_pattern: String,
    pub extension: String,
}

impl Rotation {
    /// `PhysicalCopyRotateNow`: a file with data that reached a limit.
    pub fn rotate_now(&self, bytes: usize, row_groups: usize, has_data: bool) -> bool {
        has_data
            && (self.file_size_bytes.is_some_and(|limit| bytes >= limit)
                || self.row_groups_per_file.is_some_and(|limit| row_groups >= limit))
    }
}

/// `FilenamePattern::CreateFilename`: `{i}` is the number of the file, `{uuid}` a new UUID; a pattern of neither
/// gets the number at its end.
pub fn file_name(directory: &str, pattern: &str, extension: &str, offset: usize) -> String {
    let has_placeholder = ["{i}", "{uuid}", "{uuidv4}", "{uuidv7}"]
        .iter()
        .any(|p| pattern.contains(p));
    let mut name = pattern.to_string();
    // only a pattern that is all literal gets the number at its end; an empty one is the bare extension
    if !has_placeholder && !pattern.is_empty() {
        name += "{i}";
    }
    let name = name.replace("{i}", &offset.to_string());
    let mut result = String::new();
    let mut rest = name.as_str();
    while let Some(at) = rest.find("{uuid") {
        result += &rest[..at];
        let end = rest[at..].find('}').map_or(rest.len(), |e| at + e + 1);
        result += &crate::function::generic::random_uuid_text();
        rest = &rest[end..];
    }
    result += rest;
    format!("{directory}/{result}.{extension}")
}

/// `HivePartitioning::EscapeValue`: a value spelled like the NULL directory gets a directory of its own.
pub fn hive_escape_value(text: &str) -> String {
    let result = hive_escape(text);
    if !result.eq_ignore_ascii_case("__HIVE_DEFAULT_PARTITION__") {
        return result;
    }
    format!("%{:02X}{}", result.as_bytes()[0], &result[1..])
}

/// `HivePartitioning::Escape`
pub fn hive_escape(text: &str) -> String {
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
    /// by the directory of a partition, or `""` without partitions, and the thread with PER_THREAD_OUTPUT
    writers: Mutex<OutputFiles<ParquetFileWriter>>,
    rows: AtomicU64,
    /// with RETURN_STATS: the files written, and the partition values of the files being written by path
    written: Mutex<WrittenFiles>,
    buffers: Mutex<PartitionBuffers>,
}

/// `DelayedPartitionBuffers`: the rows of each partition, until there are enough to write.
#[derive(Debug, Default)]
pub struct PartitionBuffers {
    buffers: HashMap<String, (Vec<DataChunk>, usize)>,
    /// the directories, in the order their first rows came
    order: Vec<String>,
}

impl PartitionBuffers {
    /// Buffers `rows` of `chunk` for `directory`; the rows of the partition, once they reach `threshold`.
    pub fn add(
        &mut self,
        directory: &str,
        chunk: &DataChunk,
        rows: &[usize],
        threshold: usize,
    ) -> Result<Option<Vec<DataChunk>>> {
        if !self.buffers.contains_key(directory) {
            self.order.push(directory.to_string());
        }
        let (pieces, count) = self.buffers.entry(directory.to_string()).or_default();
        let types = chunk.types();
        let mut row_values = Vec::with_capacity(types.len());
        for &row in rows {
            if pieces.last().is_none_or(|piece| piece.len() == piece.capacity()) {
                pieces.push(DataChunk::new(&types));
            }
            row_values.clear();
            row_values.extend((0..types.len()).map(|column| chunk.get_value(column, row)));
            pieces.last_mut().expect("just pushed").append_row(&row_values)?;
            *count += 1;
        }
        if *count < threshold.max(1) {
            return Ok(None);
        }
        *count = 0;
        Ok(Some(std::mem::take(pieces)))
    }

    /// The rows still buffered, by directory in the order they came.
    pub fn drain(&mut self) -> Vec<(String, Vec<DataChunk>)> {
        let order = std::mem::take(&mut self.order);
        order
            .into_iter()
            .filter_map(|directory| {
                let (pieces, _) = self.buffers.remove(&directory)?;
                (!pieces.is_empty()).then_some((directory, pieces))
            })
            .collect()
    }
}

/// The files a COPY TO has open, and how many each directory got (`last_file_offset`).
#[derive(Debug)]
pub struct OutputFiles<W> {
    pub open: HashMap<String, W>,
    pub offsets: HashMap<String, usize>,
    pub created: usize,
    /// the paths of the files, in the order they were created
    pub paths: Vec<String>,
    /// the keys of the open files, in the order they were opened
    pub opened: Vec<String>,
}

impl<W> Default for OutputFiles<W> {
    fn default() -> Self {
        Self {
            open: HashMap::new(),
            offsets: HashMap::new(),
            created: 0,
            paths: Vec::new(),
            opened: Vec::new(),
        }
    }
}

impl<W> OutputFiles<W> {
    /// Opens `writer` for `key`; with more than `limit` open, takes out those opened first, to be closed.
    pub fn insert_limited(&mut self, key: String, writer: W, limit: Option<usize>) -> Vec<W> {
        self.opened.retain(|k| *k != key && self.open.contains_key(k));
        self.opened.push(key.clone());
        self.open.insert(key, writer);
        let mut evicted = Vec::new();
        let Some(limit) = limit else {
            return evicted;
        };
        while self.open.len() > limit.max(1) && self.opened.len() > 1 {
            let oldest = self.opened.remove(0);
            if let Some(writer) = self.open.remove(&oldest) {
                evicted.push(writer);
            }
        }
        evicted
    }

    /// The key of the file of a partition's `directory` for this thread.
    pub fn key(directory: &str, rotation: Option<&Rotation>) -> String {
        match rotation {
            Some(rotation) if rotation.per_thread => format!("{directory}#{:?}", std::thread::current().id()),
            _ => directory.to_string(),
        }
    }

    /// The path of the next file of `directory` (`""` for the target itself).
    pub fn next_path(
        &mut self,
        target: &str,
        directory: &str,
        partitioning: Option<&Partitioning>,
        rotation: Option<&Rotation>,
    ) -> Result<String> {
        self.created += 1;
        let path = self.path_of(target, directory, partitioning, rotation)?;
        self.paths.push(path.clone());
        Ok(path)
    }

    fn path_of(
        &mut self,
        target: &str,
        directory: &str,
        partitioning: Option<&Partitioning>,
        rotation: Option<&Rotation>,
    ) -> Result<String> {
        match (partitioning, rotation) {
            (Some(partitioning), _) => {
                let (directory, offset_key) = if partitioning.hive_file_pattern {
                    (directory, directory)
                } else {
                    (target, "")
                };
                let offset = self.offsets.entry(offset_key.to_string()).or_insert(0);
                let path = partitioning.create_file_path_at(directory, *offset)?;
                *offset += 1;
                Ok(path)
            }
            (None, Some(rotation)) => {
                let offset = self.offsets.entry(String::new()).or_insert(0);
                let path = file_name(target, &rotation.filename_pattern, &rotation.extension, *offset);
                *offset += 1;
                Ok(path)
            }
            (None, None) => Ok(target.to_string()),
        }
    }
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
            writers: Mutex::new(OutputFiles::default()),
            rows: AtomicU64::new(0),
            written: Mutex::new((Vec::new(), HashMap::new())),
            buffers: Mutex::new(PartitionBuffers::default()),
        }
    }

    /// Writes the buffered rows of a partition.
    fn write_pieces(&self, directory: &str, pieces: Vec<DataChunk>) -> Result<()> {
        let ordered = self.options.partitioning.as_ref().filter(|p| !p.order.is_empty());
        let pieces = match ordered {
            Some(partitioning) => partitioning.sorted(pieces)?,
            None => pieces,
        };
        for piece in &pieces {
            let rows: Vec<usize> = (0..piece.len()).collect();
            self.write_rows(directory, piece, &rows)?;
        }
        // a sorted run is a file of its own
        if ordered.is_some() {
            let key = OutputFiles::<ParquetFileWriter>::key(directory, self.options.rotation.as_ref());
            let writer = self.writers.lock().open.remove(&key);
            if let Some(writer) = writer {
                self.finish(writer)?;
            }
        }
        Ok(())
    }

    /// `ParquetWriter::Finalize`, keeping what RETURN_STATS returns of the file.
    fn finish(&self, writer: ParquetFileWriter) -> Result<()> {
        let path = writer.path().to_string();
        let finished = writer.finish()?;
        if let Some(stats) = finished.stats {
            let mut written = self.written.lock();
            let partition_keys = written.1.remove(&path);
            written.0.push(WrittenFile {
                path,
                stats,
                partition_keys,
            });
        }
        Ok(())
    }

    /// The partition values of the rows of `directory`, as text, by the names of their columns.
    fn partition_keys(&self, chunk: &DataChunk, row: usize) -> Result<Vec<(String, String)>> {
        let Some(partitioning) = &self.options.partitioning else {
            return Ok(Vec::new());
        };
        partitioning
            .columns
            .iter()
            .map(|&column| {
                let value = chunk.get_value(column, row);
                Ok((self.names[column].clone(), value.try_to_string()?))
            })
            .collect()
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
        let mut options = self.options.writer.clone();
        options.not_null_columns = columns
            .iter()
            .map(|&c| self.options.not_null_columns.get(c).copied().unwrap_or(false))
            .collect();
        ParquetFileWriter::create(path, &names, &types, options)
    }

    /// Writes `rows` of `chunk` to the file of `directory`, starting new files where the rotation says.
    fn write_rows(&self, directory: &str, chunk: &DataChunk, rows: &[usize]) -> Result<()> {
        let rotation = self.options.rotation.as_ref();
        let key = OutputFiles::<ParquetFileWriter>::key(directory, rotation);
        let mut files = self.writers.lock();
        let mut done = 0;
        while done < rows.len() {
            if !files.open.contains_key(&key) {
                let path = files.next_path(&self.path, directory, self.options.partitioning.as_ref(), rotation)?;
                // an ORDER_BY without PARTITION_BY has no keys
                if self.options.output.return_stats
                    && self
                        .options
                        .partitioning
                        .as_ref()
                        .is_some_and(|p| !p.columns.is_empty())
                {
                    let keys = self.partition_keys(chunk, rows[done])?;
                    self.written.lock().1.insert(path.clone(), keys);
                }
                let writer = self.create(&path)?;
                let limit = self.options.partitioning.as_ref().map(|p| p.max_open_files);
                for evicted in files.insert_limited(key.clone(), writer, limit) {
                    self.finish(evicted)?;
                }
            }
            let writer = files.open.get_mut(&key).expect("just opened");
            // with a rotation, a file may end at any row group
            let take = match rotation {
                Some(r) if r.file_size_bytes.is_some() || r.row_groups_per_file.is_some() => {
                    writer.rows_until_row_group().min(rows.len() - done)
                }
                _ => rows.len() - done,
            };
            let piece = &rows[done..done + take];
            let columns: Vec<Vec<Value>> = self
                .written_columns()
                .into_iter()
                .map(|column| piece.iter().map(|&row| chunk.get_value(column, row)).collect())
                .collect();
            writer.append(columns)?;
            done += take;
            if let Some(rotation) = rotation
                && rotation.rotate_now(writer.bytes_written(), writer.row_group_count(), writer.has_data())
            {
                let writer = files.open.remove(&key).expect("open");
                self.finish(writer)?;
            }
        }
        Ok(())
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
                    let ready = self
                        .buffers
                        .lock()
                        .add(&directory, chunk, &rows, partitioning.flush_threshold)?;
                    if let Some(pieces) = ready {
                        self.write_pieces(&directory, pieces)?;
                    }
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
        let buffered = self.buffers.lock().drain();
        for (directory, pieces) in buffered {
            self.write_pieces(&directory, pieces)?;
        }
        let mut files = self.writers.lock();
        let rotation = self.options.rotation.as_ref();
        if files.created == 0 && !self.options.output.write_empty_file {
            return Ok(());
        }
        if files.created == 0 {
            if self.options.partitioning.is_some() {
                std::fs::create_dir_all(&self.path).map_err(|e| {
                    Error::new(ErrorKind::Io, format!("Cannot create directory \"{}\": {e}", self.path))
                })?;
                return Ok(());
            }
            let path = files.next_path(&self.path, "", None, rotation)?;
            let writer = self.create(&path)?;
            files.open.insert(String::new(), writer);
        }
        let mut keys: Vec<String> = files.open.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let writer = files.open.remove(&key).expect("listed");
            self.finish(writer)?;
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
        if self.options.output.return_stats {
            let mut written = self.written.lock();
            written.0.sort_by(|a, b| a.path.cmp(&b.path));
            let chunk = self.options.output.stats_result(&written.0)?;
            return Ok((!chunk.is_empty()).then_some(chunk));
        }
        let files = self.writers.lock();
        self.options
            .output
            .result(self.rows.load(Ordering::Relaxed), &files.paths)
            .map(Some)
    }
}

/// `is_min_value_exact` / `is_max_value_exact`, which a file sets only with the bound itself.
fn exact_or_null(stats: Option<&Statistics>, min: bool) -> String {
    match stats {
        Some(stats) if min && stats.min_bytes_opt().is_some() => stats.min_is_exact().to_string(),
        Some(stats) if !min && stats.max_bytes_opt().is_some() => stats.max_is_exact().to_string(),
        _ => "NULL::BOOLEAN".to_string(),
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
            .with_metadata_options(Some(
                parquet::file::metadata::ParquetMetaDataOptions::new().with_encoding_stats_as_mask(false),
            ))
            .parse_and_finish(&file)
            .map_err(|e| read_error(path, "Invalid Parquet file", e))
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

/// `ParquetReader::GetPartitionStats` of the one file of a scan: per row group, its rows, the zone of each column
/// of the file, and whether all the columns' bounds are exact (`ParquetPartitionRowGroup::MinMaxIsExact`).
#[allow(clippy::type_complexity, reason = "per row group: its index, its zones and whether it is complete")]
pub fn partition_statistics(scan: &ParquetScanData) -> Option<Vec<(u64, Vec<Option<crate::catalog::ColumnZone>>, bool)>> {
    let [file] = scan.files.as_slice() else {
        return None;
    };
    let (metadata, _) = read_metadata(file).ok()?;
    let leaves: Vec<Option<usize>> = (0..scan.names.len())
        .map(|column| statistics_leaf(scan, &metadata, column, file))
        .collect();
    let groups = metadata
        .row_groups()
        .iter()
        .map(|group| {
            let zones = leaves
                .iter()
                .enumerate()
                .map(|(at, leaf)| group_zone(&metadata, group, (*leaf)?, &scan.types[at]).filter(|z| !z.unknown))
                .collect();
            let exact = leaves.iter().all(|leaf| {
                leaf.and_then(|leaf| group.column(leaf).statistics())
                    .is_some_and(|s| s.min_is_exact() && s.max_is_exact())
            });
            (group.num_rows().max(0) as u64, zones, exact)
        })
        .collect();
    Some(groups)
}

/// `MultiFileFunction::MultiFileCardinality`'s `max_cardinality`: the rows of the row groups of each
/// file it opened (`ReadSingleParquetFileCardinality`; the footer's total is not what the scan
/// reads), which is the one file, or all of them with `union_by_name`.
pub fn scan_max_cardinality(scan: &ParquetScanData) -> Option<u64> {
    if scan.files.is_empty() || !all_files_opened(scan) {
        return None;
    }
    scan.files.iter().try_fold(0u64, |total, file| {
        let (metadata, _) = read_metadata(file).ok()?;
        metadata
            .row_groups()
            .iter()
            .try_fold(total, |total, group| total.checked_add(u64::try_from(group.num_rows()).ok()?))
    })
}

/// `MultiFileCardinality`: the rows of the one file, or of all files when the bind opened them all; otherwise
/// `ParquetMultiFileInfo::GetCardinality`, the rows of the first for each file, at least 1000 (there are no file
/// sizes to tell more by).
pub fn scan_cardinality(scan: &ParquetScanData) -> Option<u64> {
    let Some(first) = scan.files.first() else {
        return Some(0);
    };
    let (metadata, _) = read_metadata(first).ok()?;
    let rows = metadata.file_metadata().num_rows().max(0) as u64;
    if scan.files.len() == 1 {
        return Some(rows);
    }
    // the files opened at bind: their footers' totals
    if all_files_opened(scan) {
        let mut total = 0u64;
        for file in &scan.files {
            let (metadata, _) = read_metadata(file).ok()?;
            total = total.checked_add(metadata.file_metadata().num_rows().max(0) as u64)?;
        }
        return Some(total);
    }
    Some(rows.max(1000) * scan.files.len() as u64)
}

/// Whether the bind opened every file of the scan (`union_readers` of them all): one file, `union_by_name`, or no
/// more than `maximum_files_for_stats`. Only then do their statistics and row counts describe the scan.
fn all_files_opened(scan: &ParquetScanData) -> bool {
    scan.files.len() <= 1
        || scan.union_by_name
        || (scan.files_for_stats > 0 && scan.files.len() <= scan.files_for_stats)
}

/// `ParquetReader::ReadStatistics` over the files of a scan: what the footers say of each column from the
/// files, `None` where they don't say it.
pub fn scan_statistics(scan: &ParquetScanData) -> Vec<Option<crate::catalog::ColumnStatistics>> {
    let unknown = vec![None; scan.names.len()];
    if !all_files_opened(scan) {
        return unknown;
    }
    let mut result: Vec<Option<crate::catalog::ColumnStatistics>> = Vec::new();
    let mut first = true;
    for path in &scan.files {
        let Ok((metadata, _)) = read_metadata(path) else {
            return unknown;
        };
        // an empty file of several has no statistics, and no values the merged bounds must cover
        let rows: i64 = metadata.row_groups().iter().map(|group| group.num_rows()).sum();
        if scan.files.len() > 1 && rows == 0 {
            continue;
        }
        let mut file_stats = Vec::with_capacity(scan.names.len());
        for column in 0..scan.names.len() {
            if scan.types[column].id() == LogicalTypeId::Variant {
                file_stats.push(variant_stats::variant_statistics(&metadata, &scan.names[column], None));
                continue;
            }
            let Some(leaf) = statistics_leaf(scan, &metadata, column, path) else {
                file_stats.push(None);
                continue;
            };
            let mut stats = crate::catalog::ColumnStatistics {
                lists: None,
                variant: None,
                min: None,
                max: None,
                can_be_null: false,
                can_be_valid: false,
                geometry: None,
                strings: None,
                distinct: None,
                children: Vec::new(),
            };
            let mut bounds_known = true;
            let mut usable = true;
            // `StringStats` of each row group (`TransformParquetStatistics`), merged
            let string_column = matches!(scan.types[column].id(), LogicalTypeId::Varchar | LogicalTypeId::Blob);
            let mut strings: Option<crate::catalog::StringStatistics> = None;
            for group in metadata.row_groups() {
                let Some(zone) = group_zone(&metadata, group, leaf, &scan.types[column]) else {
                    usable = false;
                    break;
                };
                if string_column {
                    let group_strings = row_group_strings(&zone, scan.types[column].id() == LogicalTypeId::Varchar);
                    strings = Some(match strings {
                        Some(merged) => merged.merge(&group_strings, false),
                        None => group_strings,
                    });
                }
                stats.can_be_null |= zone.has_null;
                stats.can_be_valid |= zone.min.is_some() || zone.unknown;
                match (zone.min, zone.max) {
                    _ if zone.unknown => bounds_known = false,
                    (Some(min), Some(max)) if bounds_known => {
                        let less = |a: &Value, b: &Value| {
                            crate::optimizer::statistics::compare_values(
                                duckrs_parser::ast::ComparisonOp::LessThan,
                                a,
                                b,
                            ) == Some(true)
                        };
                        if stats.min.as_ref().is_none_or(|current| less(&min, current)) {
                            stats.min = Some(min);
                        }
                        if stats.max.as_ref().is_none_or(|current| less(current, &max)) {
                            stats.max = Some(max);
                        }
                    }
                    // only NULLs
                    _ => {}
                }
            }
            if !bounds_known {
                stats.min = None;
                stats.max = None;
            }
            // `TransformParquetStatistics` of strings: the bounds as the file has them, nothing of their lengths or
            // characters
            let ty = scan.types.get(column).map(LogicalType::id);
            if matches!(ty, Some(LogicalTypeId::Varchar | LogicalTypeId::Blob)) {
                let varchar = ty == Some(LogicalTypeId::Varchar);
                let mut bounds: Option<(Vec<u8>, Vec<u8>)> = None;
                let mut known = true;
                for group in metadata.row_groups() {
                    let Some(statistics) = group.column(leaf).statistics() else {
                        known = false;
                        break;
                    };
                    if statistics
                        .null_count_opt()
                        .is_some_and(|nulls| nulls >= group.num_rows() as u64)
                    {
                        continue;
                    }
                    // `StringStatsAreValid`: exact bounds of a VARCHAR that are not UTF-8 are bogus; cut ones may be
                    let valid = |bytes: &[u8], exact: bool| !(varchar && exact && std::str::from_utf8(bytes).is_err());
                    match (statistics.min_bytes_opt(), statistics.max_bytes_opt()) {
                        (Some(min), Some(max))
                            if valid(min, statistics.min_is_exact()) && valid(max, statistics.max_is_exact()) =>
                        {
                            let (low, high) = bounds.get_or_insert_with(|| (min.to_vec(), max.to_vec()));
                            if min < low.as_slice() {
                                *low = min.to_vec();
                            }
                            if max > high.as_slice() {
                                *high = max.to_vec();
                            }
                        }
                        _ => {
                            known = false;
                            break;
                        }
                    }
                }
                stats.strings = Some(crate::catalog::StringStatistics {
                    bounds: bounds.filter(|_| known),
                    has_unicode: true,
                    min_length: None,
                    max_length: None,
                    total_length: None,
                });
            }
            file_stats.push(usable.then_some(stats));
        }
        if first {
            result = file_stats;
            first = false;
            continue;
        }
        // `MergeStatistics`
        for (merged, file) in result.iter_mut().zip(file_stats) {
            *merged = match (merged.take(), file) {
                (Some(mut a), Some(b)) => {
                    a.can_be_null |= b.can_be_null;
                    a.can_be_valid |= b.can_be_valid;
                    // `VariantStats::Merge`
                    a.variant = variant_stats::merge_catalog(a.variant.take(), b.variant.clone());
                    let less = |x: &Value, y: &Value| {
                        crate::optimizer::statistics::compare_values(duckrs_parser::ast::ComparisonOp::LessThan, x, y)
                            == Some(true)
                    };
                    a.min = match (a.min, b.min) {
                        (Some(x), Some(y)) => Some(if less(&y, &x) { y } else { x }),
                        (x, None) if !b.can_be_valid => x,
                        (None, y) if !a.can_be_valid => y,
                        _ => None,
                    };
                    a.max = match (a.max, b.max) {
                        (Some(x), Some(y)) => Some(if less(&x, &y) { y } else { x }),
                        (x, None) if !b.can_be_valid => x,
                        (None, y) if !a.can_be_valid => y,
                        _ => None,
                    };
                    // `StringStats::Merge`: the bounds of both, of files that have any values
                    a.strings = match (a.strings.take(), b.strings) {
                        (Some(mut x), Some(y)) => {
                            x.bounds = match (x.bounds, y.bounds) {
                                (Some((low, high)), Some((other_low, other_high))) => {
                                    Some((low.min(other_low), high.max(other_high)))
                                }
                                (bounds, None) if !b.can_be_valid => bounds,
                                (None, bounds) if !a.can_be_valid => bounds,
                                _ => None,
                            };
                            x.has_unicode |= y.has_unicode;
                            Some(x)
                        }
                        _ => None,
                    };
                    Some(a)
                }
                _ => None,
            };
        }
    }
    if result.len() != scan.names.len() {
        return unknown;
    }
    result
}

/// `StringStats::CreateUnknown`: no bounds or lengths, and maybe unicode.
fn unknown_strings() -> crate::catalog::StringStatistics {
    crate::catalog::StringStatistics {
        bounds: None,
        has_unicode: true,
        min_length: None,
        max_length: None,
        total_length: None,
    }
}

/// The string statistics of a row group: its exact bounds, a constant where they are the same
/// (`StringStats::FromConstant`); none without exact bounds, or with only NULLs.
fn row_group_strings(zone: &crate::catalog::ColumnZone, varchar: bool) -> crate::catalog::StringStatistics {
    let bytes = |value: &Option<Value>| match value.as_ref().map(|v| v.data()) {
        Some(ValueData::Bytes(bytes)) => Some(bytes.clone()),
        _ => None,
    };
    match (bytes(&zone.min), bytes(&zone.max)) {
        (Some(min), Some(max)) if !zone.unknown => {
            if min == max {
                crate::catalog::StringStatistics::of_constant(&min, varchar)
            } else {
                crate::catalog::StringStatistics {
                    bounds: Some((min, max)),
                    ..unknown_strings()
                }
            }
        }
        _ => unknown_strings(),
    }
}

/// The leaf of the file that the scan's column `column` reads and whose statistics are its own: a top-level column
/// that is a leaf, not overridden by a hive partition, and not repaired as it is read.
/// With a `schema`, the column is the file's column of the name its key gives (`MatchesPhysicalColumn`): not
/// one found by field id, a nested one, or one the file stores as another type.
fn statistics_leaf(scan: &ParquetScanData, metadata: &ParquetMetaData, column: usize, path: &str) -> Option<usize> {
    let (name, ty) = match &scan.schema {
        Some(schema) => {
            let defined = schema.get(column)?;
            let crate::execution::parquet_schema::SchemaKey::Name(name) = &defined.key else {
                return None;
            };
            if !defined.children.is_empty() || stored_type(scan, path, name).as_ref() != Some(&defined.ty) {
                return None;
            }
            (name, &defined.ty)
        }
        None => (scan.names.get(column)?, &scan.types[column]),
    };
    if scan
            .hive_overrides
            .iter()
            .any(|(_, key)| key.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let descriptor = metadata.file_metadata().schema_descr();
    let leaf = (0..descriptor.num_columns()).find(|&i| {
        let path = descriptor.column(i).path().parts().to_vec();
        path.len() == 1 && path[0].eq_ignore_ascii_case(name)
    })?;
    // repaired strings no longer lie within the bounds of the stored bytes (`StringColumnType::VARCHAR`; JSON, which
    // the file marks as such, is validated but not repaired)
    let json = ty.alias() == Some("JSON")
        || descriptor.column(leaf).converted_type() == parquet::basic::ConvertedType::JSON
        || matches!(descriptor.column(leaf).logical_type_ref(), Some(parquet::basic::LogicalType::Json));
    if scan.utf8_validation.repairs() && ty.id() == LogicalTypeId::Varchar && !json {
        return None;
    }
    Some(leaf)
}

/// The leaf that holds the elements of the LIST column `column`, or the values of the MAP column, with their type:
/// `None` for other columns, or elements that are nested themselves.
fn element_leaf(
    scan: &ParquetScanData,
    metadata: &ParquetMetaData,
    column: usize,
    path: &str,
) -> Option<(usize, LogicalType)> {
    let ty = scan.types.get(column)?;
    let element = match ty.id() {
        LogicalTypeId::List => ty.child_type()?.clone(),
        LogicalTypeId::Map => ty.child_type()?.struct_children().get(1)?.1.clone(),
        _ => return None,
    };
    if scan.schema.is_some() || element.is_nested() {
        return None;
    }
    let name = scan.names.get(column)?;
    if scan.hive_overrides.iter().any(|(_, key)| key.eq_ignore_ascii_case(name)) {
        return None;
    }
    if stored_type(scan, path, name).as_ref() != Some(ty) {
        return None;
    }
    let descriptor = metadata.file_metadata().schema_descr();
    let leaves: Vec<usize> = (0..descriptor.num_columns())
        .filter(|&i| {
            let column = descriptor.column(i);
            let parts = column.path().parts();
            parts.len() == 3 && parts[0].eq_ignore_ascii_case(name)
        })
        .collect();
    // a list has one leaf, a map its keys and then its values
    let leaf = match (ty.id(), leaves.as_slice()) {
        (LogicalTypeId::List, [leaf]) | (LogicalTypeId::Map, [_, leaf]) => *leaf,
        _ => return None,
    };
    if scan.utf8_validation.repairs() && element.id() == LogicalTypeId::Varchar {
        return None;
    }
    Some((leaf, element))
}

/// The DuckDB type the file at `path` stores its top-level column `name` as.
fn stored_type(scan: &ParquetScanData, path: &str, name: &str) -> Option<LogicalType> {
    let builder = open_builder(path).ok()?;
    let special = special_columns(&builder, scan.geoparquet_conversion).ok()?;
    let schema = Arc::clone(builder.schema());
    let index = schema.fields().iter().position(|field| field.name().eq_ignore_ascii_case(name))?;
    column_type(schema.field(index), &special, index, scan.binary_as_string, scan.int96_as).ok()
}

/// What the statistics of row group `group` say of the scan's column `column`, stored in `leaf`: `None` where
/// there are none, `unknown` where they have no bounds DuckDB can use.
fn group_zone(
    metadata: &ParquetMetaData,
    group: &parquet::file::metadata::RowGroupMetaData,
    leaf: usize,
    ty: &LogicalType,
) -> Option<crate::catalog::ColumnZone> {
    use parquet::basic::Type as Physical;
    let descriptor = metadata.file_metadata().schema_descr().column(leaf);
    let statistics = group.column(leaf).statistics()?;
    let rows = group.num_rows() as u64;
    let nulls = statistics.null_count_opt()?;
    let mut zone = crate::catalog::ColumnZone {
        has_null: nulls > 0,
        ..crate::catalog::ColumnZone::default()
    };
    if nulls >= rows {
        return Some(zone);
    }
    // NaNs are left out of float bounds, but are the largest values to DuckDB; INT96 bounds compare bytes
    let bounded = !matches!(
        descriptor.physical_type(),
        Physical::FLOAT | Physical::DOUBLE | Physical::INT96
    ) && !matches!(ty.id(), LogicalTypeId::Float | LogicalTypeId::Double);
    // byte arrays with only the deprecated bounds may be ordered as signed bytes
    let deprecated = statistics.is_min_max_deprecated()
        && matches!(
            descriptor.physical_type(),
            Physical::BYTE_ARRAY | Physical::FIXED_LEN_BYTE_ARRAY
        );
    let bound = |bytes: Option<&[u8]>, exact: bool| -> Option<Value> {
        let bytes = bytes?;
        // `StringStatsAreValid`: exact bounds of a VARCHAR that are not UTF-8 are bogus
        if ty.id() == LogicalTypeId::Varchar && std::str::from_utf8(bytes).is_err() {
            return None;
        }
        let text = statistic_text(&descriptor, bytes)?;
        if !exact || deprecated {
            return None;
        }
        try_cast_value(&Value::varchar(text), ty)
    };
    let min = bound(statistics.min_bytes_opt(), statistics.min_is_exact());
    let max = bound(statistics.max_bytes_opt(), statistics.max_is_exact());
    match (min, max) {
        (Some(min), Some(max)) if bounded => {
            zone.min = Some(min);
            zone.max = Some(max);
        }
        _ => zone.unknown = true,
    }
    Some(zone)
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
                        shown(time_tz_value(micros))
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
                (Some(Logical::Time(time)), _) if time.unit == Unit::NANOS && !time.is_adjusted_to_u_t_c => {
                    (0..=1000 * MICROS_PER_DAY).contains(&value).then(|| {
                        shown(Value::new(LogicalType::new(LogicalTypeId::TimeNs), ValueData::Int64(value)).ok()?)
                    })?
                }
                (Some(Logical::Time(time)), _) => {
                    let micros = match time.unit {
                        Unit::NANOS if time.is_adjusted_to_u_t_c => value.rem_euclid(1000 * MICROS_PER_DAY) / 1000,
                        Unit::NANOS => value / 1000,
                        _ => value,
                    };
                    if time.is_adjusted_to_u_t_c {
                        shown(time_tz_value(micros))
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
    let raw = raw_chunks(path);
    let mut rows = Vec::new();
    for (group, row_group) in metadata.row_groups().iter().enumerate() {
        for (column, chunk) in row_group.columns().iter().enumerate() {
            let (_, _, nulls, distinct) = chunk.statistics().map_or((None, None, None, None), statistics_text);
            let descriptor = chunk.column_descr();
            let stat = |bytes: Option<&[u8]>| bytes.and_then(|b| statistic_text(descriptor, b));
            let raw_chunk = raw.as_ref().and_then(|raw| raw.get(group)?.get(column).cloned());
            let (min, max, min_value, max_value) = match &raw_chunk {
                // `stats_min` / `stats_max` are the deprecated bounds, `stats_min_value` / ... the others
                Some(raw) => (
                    stat(raw.min.as_deref()),
                    stat(raw.max.as_deref()),
                    stat(raw.min_value.as_deref()),
                    stat(raw.max_value.as_deref()),
                ),
                None => {
                    let min = stat(chunk.statistics().and_then(Statistics::min_bytes_opt));
                    let max = stat(chunk.statistics().and_then(Statistics::max_bytes_opt));
                    (min.clone(), max.clone(), min, max)
                }
            };
            let encodings: Vec<String> = raw_chunk
                .and_then(|raw| raw.encodings)
                .unwrap_or_else(|| chunk.encodings().map(|e| e.to_string()).collect());
            let encoding_stats = match chunk.page_encoding_stats() {
                Some(all) => {
                    let items: Vec<String> = all
                        .iter()
                        .map(|stat| {
                            format!(
                                "{{'page_type': {}, 'encoding': {}, 'count': {}}}",
                                sql_text(&format!("{:?}", stat.page_type)),
                                sql_text(&stat.encoding.to_string()),
                                stat.count
                            )
                        })
                        .collect();
                    format!("[{}]::{ENCODING_STATS_TYPE}", items.join(", "))
                }
                None => format!("NULL::{ENCODING_STATS_TYPE}"),
            };
            let nan_count = chunk.statistics().and_then(Statistics::nan_count_opt).map(|n| n as i64);
            let (geo_bbox, geo_types) = geo_statistics_text(chunk.geo_statistics())?;
            rows.push(format!(
                "({}, {group}, {}, {}, {}, {column}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, NULL::BIGINT, {}, {}, \
                 {}, {}, NULL::MAP(BLOB, BLOB), {}, {}, {}, {}, {}, {geo_bbox}, {geo_types}, {}, {}, {}, \
                 {}, {}, {})",
                sql_text(path),
                row_group.num_rows(),
                row_group.num_columns(),
                row_group.total_byte_size(),
                chunk.file_offset(),
                chunk.num_values(),
                sql_text(&chunk.column_path().parts().join(", ")),
                sql_text(&chunk.column_type().to_string()),
                sql_or_null(min),
                sql_or_null(max),
                sql_number(nulls),
                sql_number(distinct),
                sql_or_null(min_value),
                sql_or_null(max_value),
                sql_text(chunk.compression().to_string().split('(').next().unwrap_or_default()),
                sql_text(&encodings.join(", ")),
                sql_number(chunk.dictionary_page_offset()),
                chunk.data_page_offset(),
                chunk.compressed_size(),
                chunk.uncompressed_size(),
                sql_number(chunk.bloom_filter_offset()),
                sql_number(chunk.bloom_filter_length().map(i64::from)),
                exact_or_null(chunk.statistics(), true),
                exact_or_null(chunk.statistics(), false),
                row_group.compressed_size(),
                encoding_stats,
                sql_number(nan_count),
                sql_number(chunk.column_index_offset()),
                sql_number(chunk.column_index_length().map(i64::from)),
                sql_number(chunk.offset_index_offset()),
                sql_number(chunk.offset_index_length().map(i64::from)),
            ));
        }
    }
    Ok(rows)
}

// Ported from duckdb/extension/parquet/parquet_metadata.cpp (ConvertParquetGeoStatsBBOX and
// ConvertParquetGeoStatsTypes) @ rust-port-baseline
/// `geo_bbox` and `geo_types` of a column chunk, as SQL.
fn geo_statistics_text(
    stats: Option<&parquet::geospatial::statistics::GeospatialStatistics>,
) -> Result<(String, String)> {
    let double = |value: Option<f64>| match value {
        Some(value) => format!("{}::DOUBLE", sql_text(&format!("{value:?}"))),
        None => "NULL::DOUBLE".to_string(),
    };
    let bbox = match stats.and_then(|stats| stats.bounding_box()) {
        Some(bbox) => format!(
            "{{'xmin': {}, 'xmax': {}, 'ymin': {}, 'ymax': {}, 'zmin': {}, 'zmax': {}, 'mmin': {}, 'mmax': {}}}::\
             {GEO_BBOX_TYPE}",
            double(Some(bbox.get_xmin())),
            double(Some(bbox.get_xmax())),
            double(Some(bbox.get_ymin())),
            double(Some(bbox.get_ymax())),
            double(bbox.get_zmin()),
            double(bbox.get_zmax()),
            double(bbox.get_mmin()),
            double(bbox.get_mmax()),
        ),
        None => format!("NULL::{GEO_BBOX_TYPE}"),
    };
    let types = match stats.and_then(|stats| stats.geospatial_types()) {
        Some(codes) => {
            let mut set = duckrs_common::geometry::GeometryStats::empty();
            for &code in codes {
                if !(1..=7).contains(&(code % 1000)) {
                    return Err(invalid("Unsupported geometry type in Parquet geo metadata"));
                }
                if !(0..=3).contains(&(code / 1000)) {
                    return Err(invalid("Unsupported geometry vertex type in Parquet geo metadata"));
                }
                set.add_wkb_type(code);
            }
            let names: Vec<String> = set.type_names(true).iter().map(|name| sql_text(name)).collect();
            format!("[{}]::VARCHAR[]", names.join(", "))
        }
        None => "NULL::VARCHAR[]".to_string(),
    };
    Ok((bbox, types))
}

/// `ParquetLogicalTypeToString`: the Thrift `printTo` of the member of the union that is set.
fn logical_type_text(logical: &parquet::basic::LogicalType) -> Option<String> {
    use parquet::basic::{LogicalType as L, TimeUnit as U};
    let unit = |unit: &U| {
        let (millis, micros, nanos) = match unit {
            U::MILLIS => ("MilliSeconds()", "<null>", "<null>"),
            U::MICROS => ("<null>", "MicroSeconds()", "<null>"),
            U::NANOS => ("<null>", "<null>", "NanoSeconds()"),
        };
        format!("TimeUnit(MILLIS={millis}, MICROS={micros}, NANOS={nanos})")
    };
    Some(match logical {
        L::String => "StringType()".to_string(),
        L::Map => "MapType()".to_string(),
        L::List => "ListType()".to_string(),
        L::Enum => "EnumType()".to_string(),
        L::Decimal(decimal) => format!("DecimalType(scale={}, precision={})", decimal.scale, decimal.precision),
        L::Date => "DateType()".to_string(),
        L::Time(time) => format!(
            "TimeType(isAdjustedToUTC={}, unit={})",
            u8::from(time.is_adjusted_to_u_t_c),
            unit(&time.unit)
        ),
        L::Timestamp(ts) => format!(
            "TimestampType(isAdjustedToUTC={}, unit={})",
            u8::from(ts.is_adjusted_to_u_t_c),
            unit(&ts.unit)
        ),
        L::Integer(int) => format!(
            "IntType(bitWidth={}, isSigned={})",
            int.bit_width,
            u8::from(int.is_signed)
        ),
        L::Unknown => "NullType()".to_string(),
        L::Json => "JsonType()".to_string(),
        L::Bson => "BsonType()".to_string(),
        L::Uuid => "UUIDType()".to_string(),
        L::Float16 => "Float16Type()".to_string(),
        L::Geometry(geometry) => format!("GeometryType(crs={})", geometry.crs.as_deref().unwrap_or("<null>")),
        // C++ has no text for the others
        _ => return None,
    })
}

/// The footer of a file as Thrift values, for what the crate leaves out.
fn raw_footer(path: &str) -> Option<ThriftValue> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path).ok()?;
    let size = file.seek(SeekFrom::End(0)).ok()?;
    file.seek(SeekFrom::Start(size.checked_sub(8)?)).ok()?;
    let mut tail = [0u8; 8];
    file.read_exact(&mut tail).ok()?;
    if &tail[4..] != b"PAR1" {
        return None;
    }
    let length = i32::from_le_bytes(tail[..4].try_into().ok()?);
    read_thrift_struct(&mut file, size as i64 - 8 - i64::from(length), length)
}

/// What the crate leaves out of a column chunk's metadata: its encodings in the order the file lists them (the
/// crate keeps a set), and its deprecated and current bounds apart (the crate keeps one of them).
#[derive(Default, Clone)]
struct RawChunk {
    encodings: Option<Vec<String>>,
    min: Option<Vec<u8>>,
    max: Option<Vec<u8>>,
    min_value: Option<Vec<u8>>,
    max_value: Option<Vec<u8>>,
}

fn raw_chunks(path: &str) -> Option<Vec<Vec<RawChunk>>> {
    let footer = raw_footer(path)?;
    let name = |code: i64| {
        match code {
            0 => "PLAIN",
            2 => "PLAIN_DICTIONARY",
            3 => "RLE",
            4 => "BIT_PACKED",
            5 => "DELTA_BINARY_PACKED",
            6 => "DELTA_LENGTH_BYTE_ARRAY",
            7 => "DELTA_BYTE_ARRAY",
            8 => "RLE_DICTIONARY",
            9 => "BYTE_STREAM_SPLIT",
            _ => return None,
        }
        .to_string()
        .into()
    };
    let bytes = |value: Option<&ThriftValue>| match value {
        Some(ThriftValue::Binary(bytes)) => Some(bytes.clone()),
        _ => None,
    };
    footer
        .field(4)?
        .items()
        .iter()
        .map(|group| {
            Some(
                group
                    .field(1)?
                    .items()
                    .iter()
                    .map(|chunk| {
                        let Some(meta) = chunk.field(3) else {
                            return RawChunk::default();
                        };
                        let encodings = meta
                            .field(2)
                            .and_then(|codes| codes.items().iter().map(|code| name(code.int()?)).collect());
                        let stats = meta.field(12);
                        let stat = |id: i16| bytes(stats.and_then(|stats| stats.field(id)));
                        RawChunk {
                            encodings,
                            max: stat(1),
                            min: stat(2),
                            max_value: stat(5),
                            min_value: stat(6),
                        }
                    })
                    .collect(),
            )
        })
        .collect()
}

/// The precision and scale of the first DECIMAL of the schema that C++ refuses, which the crate's errors leave out.
fn invalid_decimal(path: &str) -> Option<(i64, i64)> {
    let footer = raw_footer(path)?;
    footer.field(2)?.items().iter().find_map(|element| {
        let (precision, scale) = match element.field(10).and_then(|logical| logical.field(5)) {
            Some(decimal) => (decimal.field(2)?.int()?, decimal.field(1)?.int()?),
            None if element.field(6).and_then(ThriftValue::int) == Some(5) => {
                (element.field(8)?.int()?, element.field(7)?.int()?)
            }
            None => return None,
        };
        (precision < 1 || scale < 0 || scale > precision).then_some((precision, scale))
    })
}

/// The repetition of the root of the schema, which the crate leaves out.
fn root_repetition(path: &str) -> Option<String> {
    let footer = raw_footer(path)?;
    let root = footer.field(2)?.items().first()?;
    Some(
        match root.field(3)?.int()? {
            0 => "REQUIRED",
            1 => "OPTIONAL",
            _ => "REPEATED",
        }
        .to_string(),
    )
}

/// `CollectElementTypes`: the DuckDB type of each element of the schema, in its order, a group collapsed into
/// its only child having the type of that child.
fn element_types(root: &Type, types: &[LogicalType], fields: &[Field]) -> Vec<Option<String>> {
    fn children(ty: &Type) -> &[parquet::schema::types::TypePtr] {
        if ty.is_group() { ty.get_fields() } else { &[] }
    }
    fn count(ty: &Type) -> usize {
        1 + children(ty).iter().map(|field| count(field)).sum::<usize>()
    }
    fn set(out: &mut [Option<String>], at: usize, ty: &LogicalType) {
        if out[at].is_none() {
            out[at] = Some(ty.to_string());
        }
    }
    /// `at` is the index of `node`, which has the type `ty` (and the Arrow field `arrow`, where known).
    fn assign(node: &Type, at: usize, ty: &LogicalType, arrow: Option<&Field>, out: &mut [Option<String>]) {
        set(out, at, ty);
        let fields = children(node);
        if fields.is_empty() {
            return;
        }
        match ty.id() {
            // `CollectElementTypes`: a LIST and its repeated group are one column schema, of the LIST
            LogicalTypeId::List | LogicalTypeId::Array => {
                let Some(element) = ty.child_type() else { return };
                let child = &fields[0];
                let grand = children(child);
                let collapsed = !child.is_primitive()
                    && child.get_basic_info().repetition() == parquet::basic::Repetition::REPEATED
                    && grand.len() == 1
                    && child.name() != "array"
                    && !child.name().ends_with("_tuple");
                set(out, at + 1, ty);
                if collapsed {
                    assign(&grand[0], at + 2, element, None, out);
                } else {
                    assign(child, at + 1, element, None, out);
                }
            }
            // a MAP and its `key_value` group as well
            LogicalTypeId::Map => {
                let Some(entry) = ty.child_type() else { return };
                let entries = entry.struct_children();
                let child = &fields[0];
                set(out, at + 1, ty);
                let mut next = at + 2;
                for (field, (_, field_ty)) in children(child).iter().zip(entries) {
                    assign(field, next, field_ty, None, out);
                    next += count(field);
                }
            }
            LogicalTypeId::Struct => {
                let arrow_children = match arrow.map(Field::data_type) {
                    Some(DataType::Struct(arrow_children)) => Some(arrow_children),
                    _ => None,
                };
                let mut next = at + 1;
                for (i, (field, (_, field_ty))) in fields.iter().zip(ty.struct_children()).enumerate() {
                    let arrow_child = arrow_children.and_then(|c| c.get(i)).map(|f| f.as_ref());
                    assign(field, next, field_ty, arrow_child, out);
                    next += count(field);
                }
            }
            // the `metadata`, `value` and `typed_value` of a VARIANT are read as the columns they are
            LogicalTypeId::Variant => {
                let Some(DataType::Struct(arrow_children)) = arrow.map(Field::data_type) else {
                    return;
                };
                let mut next = at + 1;
                for field in fields {
                    let arrow_child = arrow_children.iter().find(|f| f.name() == field.name());
                    if let Some(child_ty) = arrow_child.and_then(|f| logical_type(f, false).ok()) {
                        assign(field, next, &child_ty, arrow_child.map(|f| f.as_ref()), out);
                    }
                    next += count(field);
                }
            }
            _ => {}
        }
    }
    let mut out = vec![None; count(root)];
    let mut next = 1;
    for (i, (field, ty)) in root.get_fields().iter().zip(types).enumerate() {
        assign(field, next, ty, fields.get(i), &mut out);
        next += count(field);
    }
    out
}

fn schema_rows(ty: &Type, path: &str, column: &mut i64, rows: &mut Vec<String>, types: &[Option<String>]) {
    let info = ty.get_basic_info();
    let repetition = if *column == 0 {
        root_repetition(path)
    } else {
        info.has_repetition().then(|| info.repetition().to_string())
    };
    let converted = info.converted_type();
    let converted = (converted != parquet::basic::ConvertedType::NONE).then(|| converted.to_string());
    let logical = info.logical_type_ref().and_then(logical_type_text);
    let id = info.has_id().then(|| i64::from(info.id()));
    let (physical, length, scale, precision, children, id_of_column) = match ty {
        Type::PrimitiveType {
            physical_type,
            type_length,
            scale,
            precision,
            ..
        } => (
            Some(physical_type.to_string()),
            (*type_length > 0).then(|| type_length.to_string()),
            Some(i64::from(*scale)),
            Some(i64::from(*precision)),
            None,
            Some(*column),
        ),
        Type::GroupType { fields, .. } => (None, None, None, None, Some(fields.len() as i64), Some(*column)),
    };
    // `column_id`: the index of the element in the schema
    *column += 1;
    rows.push(format!(
        "({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
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
        sql_or_null(types.get(*column as usize - 1).cloned().flatten()),
        sql_number(id_of_column),
    ));
    if let Type::GroupType { fields, .. } = ty {
        for field in fields {
            schema_rows(field, path, column, rows, types);
        }
    }
}

/// `parquet_schema`: a row of SQL per element of the schema.
pub fn schema_table_rows(path: &str) -> Result<Vec<String>> {
    let (metadata, _) = read_metadata(path)?;
    let mut rows = Vec::new();
    let root = metadata.file_metadata().schema();
    let fields: Vec<Field> = open_builder(path)
        .map(|builder| builder.schema().fields().iter().map(|f| f.as_ref().clone()).collect())
        .unwrap_or_default();
    let types = file_schema(path, false, Int96As::default(), true)
        .map(|(_, types)| element_types(root, &types, &fields))
        .unwrap_or_default();
    schema_rows(root, path, &mut 0, &mut rows, &types);
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

/// The type of `encoding_stats` in `parquet_metadata`.
pub const ENCODING_STATS_TYPE: &str = "STRUCT(page_type VARCHAR, encoding VARCHAR, count INTEGER)[]";
/// The type of `geo_bbox` in `parquet_metadata`.
pub const GEO_BBOX_TYPE: &str = "STRUCT(xmin DOUBLE, xmax DOUBLE, ymin DOUBLE, ymax DOUBLE, zmin DOUBLE, zmax DOUBLE, \
                                 mmin DOUBLE, mmax DOUBLE)";

/// A value of Thrift's compact protocol, as far as the page index needs.
#[derive(Debug)]
enum ThriftValue {
    Int(i64),
    Bool(bool),
    Binary(Vec<u8>),
    List(Vec<ThriftValue>),
    Struct(Vec<(i16, ThriftValue)>),
}

impl ThriftValue {
    fn field(&self, id: i16) -> Option<&ThriftValue> {
        match self {
            ThriftValue::Struct(fields) => fields.iter().find(|(i, _)| *i == id).map(|(_, v)| v),
            _ => None,
        }
    }

    fn int(&self) -> Option<i64> {
        match self {
            ThriftValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    fn items(&self) -> &[ThriftValue] {
        match self {
            ThriftValue::List(items) => items,
            _ => &[],
        }
    }
}

struct ThriftReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl ThriftReader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }

    fn zigzag(&mut self) -> Option<i64> {
        let z = self.varint()?;
        Some(((z >> 1) as i64) ^ -((z & 1) as i64))
    }

    fn value(&mut self, kind: u8) -> Option<ThriftValue> {
        Some(match kind {
            1 => ThriftValue::Bool(true),
            2 => ThriftValue::Bool(false),
            3 => ThriftValue::Int(i64::from(self.byte()? as i8)),
            4..=6 => ThriftValue::Int(self.zigzag()?),
            7 => {
                let bytes = self.bytes.get(self.at..self.at + 8)?;
                self.at += 8;
                ThriftValue::Int(i64::from_le_bytes(bytes.try_into().ok()?))
            }
            8 => {
                let length = self.varint()? as usize;
                let bytes = self.bytes.get(self.at..self.at + length)?.to_vec();
                self.at += length;
                ThriftValue::Binary(bytes)
            }
            9 | 10 => {
                let header = self.byte()?;
                let size = if header >> 4 == 15 {
                    self.varint()? as usize
                } else {
                    (header >> 4) as usize
                };
                let element = header & 0x0f;
                let mut items = Vec::with_capacity(size.min(1 << 16));
                for _ in 0..size {
                    items.push(if element == 1 || element == 2 {
                        ThriftValue::Bool(self.byte()? == 1)
                    } else {
                        self.value(element)?
                    });
                }
                ThriftValue::List(items)
            }
            12 => self.structure()?,
            _ => return None,
        })
    }

    fn structure(&mut self) -> Option<ThriftValue> {
        let mut fields = Vec::new();
        let mut last = 0i16;
        loop {
            let header = self.byte()?;
            if header == 0 {
                return Some(ThriftValue::Struct(fields));
            }
            let id = if header >> 4 == 0 {
                self.zigzag()? as i16
            } else {
                last + i16::from(header >> 4)
            };
            last = id;
            fields.push((id, self.value(header & 0x0f)?));
        }
    }
}

/// A Thrift struct of a file, at `offset` and `length` bytes long.
fn read_thrift_struct(file: &mut File, offset: i64, length: i32) -> Option<ThriftValue> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(u64::try_from(offset).ok()?)).ok()?;
    let mut bytes = vec![0u8; usize::try_from(length).ok()?];
    file.read_exact(&mut bytes).ok()?;
    ThriftReader { bytes: &bytes, at: 0 }.structure()
}

/// `parquet_page_index`: a row of SQL for each page of the column chunks with a page index
/// (`ParquetPageIndexProcessor`).
pub fn page_index_rows(path: &str) -> Result<Vec<String>> {
    let (metadata, _) = read_metadata(path)?;
    let mut file =
        File::open(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
    let mut rows = Vec::new();
    for (group, row_group) in metadata.row_groups().iter().enumerate() {
        for (column, chunk) in row_group.columns().iter().enumerate() {
            let offset_index = match (chunk.offset_index_offset(), chunk.offset_index_length()) {
                (Some(offset), Some(length)) => read_thrift_struct(&mut file, offset, length),
                _ => None,
            };
            let column_index = match (chunk.column_index_offset(), chunk.column_index_length()) {
                (Some(offset), Some(length)) => read_thrift_struct(&mut file, offset, length),
                _ => None,
            };
            let locations = offset_index
                .as_ref()
                .and_then(|o| o.field(1))
                .map(ThriftValue::items)
                .unwrap_or(&[]);
            let null_pages = column_index
                .as_ref()
                .and_then(|c| c.field(1))
                .map(ThriftValue::items)
                .unwrap_or(&[]);
            let min_values = column_index
                .as_ref()
                .and_then(|c| c.field(2))
                .map(ThriftValue::items)
                .unwrap_or(&[]);
            let max_values = column_index
                .as_ref()
                .and_then(|c| c.field(3))
                .map(ThriftValue::items)
                .unwrap_or(&[]);
            let null_counts = column_index.as_ref().and_then(|c| c.field(5)).map(ThriftValue::items);
            let boundary = column_index
                .as_ref()
                .and_then(|c| c.field(4))
                .and_then(ThriftValue::int);
            let pages = locations.len().max(null_pages.len());
            for page in 0..pages {
                let (mut offset, mut size, mut first, mut count) = (None, None, None, None);
                if let Some(location) = locations.get(page) {
                    offset = location.field(1).and_then(ThriftValue::int);
                    size = location.field(2).and_then(ThriftValue::int);
                    first = location.field(3).and_then(ThriftValue::int);
                    let next = locations
                        .get(page + 1)
                        .and_then(|l| l.field(3))
                        .and_then(ThriftValue::int)
                        .unwrap_or(row_group.num_rows());
                    count = first.map(|f| next - f);
                }
                let has_bounds = column_index.is_some()
                    && page < null_pages.len()
                    && page < min_values.len()
                    && page < max_values.len();
                let (mut null_page, mut min, mut max, mut nulls, mut order) = (None, None, None, None, None);
                if has_bounds {
                    let is_null = matches!(null_pages[page], ThriftValue::Bool(true));
                    null_page = Some(is_null);
                    let bound = |value: &ThriftValue| match value {
                        ThriftValue::Binary(bytes) if !is_null => statistic_text(chunk.column_descr(), bytes),
                        _ => None,
                    };
                    min = bound(&min_values[page]);
                    max = bound(&max_values[page]);
                    nulls = null_counts.and_then(|n| n.get(page)).and_then(ThriftValue::int);
                    order = boundary.map(|b| match b {
                        1 => "ASCENDING",
                        2 => "DESCENDING",
                        _ => "UNORDERED",
                    });
                }
                rows.push(format!(
                    "({}, {group}, {column}, {}, {page}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
                    sql_text(path),
                    sql_text(&chunk.column_path().parts().join(", ")),
                    sql_number(offset),
                    sql_number(size),
                    sql_number(first),
                    sql_number(count),
                    null_page.map_or("NULL".to_string(), |b| b.to_string()),
                    sql_or_null(min),
                    sql_or_null(max),
                    sql_number(nulls),
                    sql_or_null(order.map(str::to_string)),
                ));
            }
        }
    }
    Ok(rows)
}

/// Whether the file says its interval bloom filters hash normalized intervals.
fn normalized_interval_blooms(metadata: &ParquetMetaData) -> bool {
    metadata.file_metadata().key_value_metadata().is_some_and(|kvs| {
        kvs.iter()
            .any(|kv| kv.key == "duckdb.interval_bloom_filter" && kv.value.as_deref() == Some("normalized-v1"))
    })
}

/// Whether the bloom filter of the column chunk rules out `value`, of the type `ty` the column is read as
/// (`ParquetStatisticsUtils::BloomFilterExcludes`).
fn bloom_excludes(
    file: &mut File,
    chunk: &parquet::file::metadata::ColumnChunkMetaData,
    ty: &LogicalType,
    value: &Value,
    normalized: bool,
) -> Result<bool> {
    let Some(offset) = chunk.bloom_filter_offset().filter(|offset| *offset > 0) else {
        return Ok(false);
    };
    let (mut probe, mut probe_type) = (value.clone(), ty.clone());
    // the unit the file stores timestamps in
    if ty.id() == LogicalTypeId::Timestamp
        && let Some(parquet::basic::LogicalType::Timestamp(ts)) = chunk.column_descr().logical_type_ref()
    {
        let micros = probe.as_i128().unwrap_or(0) as i64;
        match ts.unit {
            parquet::basic::TimeUnit::MILLIS => {
                probe_type = LogicalType::new(LogicalTypeId::TimestampMs);
                probe = Value::new(probe_type.clone(), ValueData::Int64(micros.div_euclid(1000)))?;
            }
            parquet::basic::TimeUnit::NANOS => {
                probe_type = LogicalType::new(LogicalTypeId::TimestampNs);
                probe = Value::new(probe_type.clone(), ValueData::Int64(micros.saturating_mul(1000)))?;
            }
            _ => {}
        }
    }
    let Some(hash) = crate::execution::parquet_writer::bloom_probe_hash(&probe_type, &probe, normalized)? else {
        return Ok(false);
    };
    let length = chunk.bloom_filter_length().map(i64::from);
    Ok(read_bloom_filter(file, offset as u64, length).is_some_and(|filter| !filter.contains(hash)))
}

/// `parquet_bloom_probe`: for each row group of a file, whether the bloom filter of `column` rules `value` out
/// (`ParquetBloomProbeProcessor`), as rows of SQL.
pub fn bloom_probe_rows(path: &str, column: &str, value: &Value) -> Result<Vec<String>> {
    let (metadata, _) = read_metadata(path)?;
    let (names, types) = file_schema(path, false, Int96As::Timestamp, true)?;
    let Some(index) = names.iter().position(|n| n == column) else {
        return Err(invalid(format!("Column {column} not found in {path}")));
    };
    let ty = &types[index];
    let probe = cast_value(value, ty)?;
    let normalized = normalized_interval_blooms(&metadata);
    let mut file =
        File::open(path).map_err(|e| Error::new(ErrorKind::Io, format!("Cannot open file \"{path}\": {e}")))?;
    let mut rows = Vec::new();
    for (group, row_group) in metadata.row_groups().iter().enumerate() {
        let Some(chunk) = row_group
            .columns()
            .iter()
            .find(|c| c.column_path().parts().len() == 1 && c.column_path().parts()[0] == column)
        else {
            continue;
        };
        let excludes = bloom_excludes(&mut file, chunk, ty, &probe, normalized)?;
        rows.push(format!("({}, {group}::BIGINT, {excludes})", sql_text(path)));
    }
    Ok(rows)
}

/// A bloom filter of a file: its `BloomFilterHeader`, then `numBytes` of bits.
/// The bloom filter at `offset`, of `length` bytes with its header where the column says so; `None` where it is
/// not one DuckDB can use or doesn't fit in the file (the checks of `ParquetStatisticsUtils::BloomFilterExcludes`).
fn read_bloom_filter(
    file: &mut File,
    offset: u64,
    length: Option<i64>,
) -> Option<crate::execution::parquet_writer::BloomFilter> {
    use std::io::{Read, Seek, SeekFrom};
    let file_size = file.metadata().ok()?.len();
    if offset == 0 || offset >= file_size {
        return None;
    }
    let length = match length {
        Some(length) if length <= 0 => return None,
        Some(length) => {
            let length = length as u64;
            if length > file_size || offset > file_size - length {
                return None;
            }
            Some(length)
        }
        None => None,
    };
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut header = vec![0u8; 64];
    let read = file.read(&mut header).ok()?;
    header.truncate(read);
    let mut at = 0usize;
    let mut num_bytes = None;
    // Thrift's compact protocol: field headers, then the values; only `numBytes` (1) matters
    fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = *bytes.get(*at)?;
            *at += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
        }
    }
    fn skip_struct(bytes: &[u8], at: &mut usize) -> Option<()> {
        loop {
            let byte = *bytes.get(*at)?;
            *at += 1;
            if byte == 0 {
                return Some(());
            }
            if byte >> 4 == 0 {
                varint(bytes, at)?;
            }
            match byte & 0x0f {
                1 | 2 => {}
                3 => *at += 1,
                4..=6 => {
                    varint(bytes, at)?;
                }
                8 => {
                    let length = varint(bytes, at)? as usize;
                    *at += length;
                }
                12 => skip_struct(bytes, at)?,
                _ => return None,
            }
        }
    }
    let mut last = 0i64;
    loop {
        let byte = *header.get(at)?;
        at += 1;
        if byte == 0 {
            break;
        }
        let id = if byte >> 4 == 0 {
            let z = varint(&header, &mut at)?;
            ((z >> 1) as i64) ^ -((z & 1) as i64)
        } else {
            last + i64::from(byte >> 4)
        };
        last = id;
        match byte & 0x0f {
            5 if id == 1 => {
                let z = varint(&header, &mut at)?;
                num_bytes = Some((((z >> 1) as i64) ^ -((z & 1) as i64)) as usize);
            }
            5 | 6 => {
                varint(&header, &mut at)?;
            }
            12 => skip_struct(&header, &mut at)?,
            _ => return None,
        }
    }
    let size = num_bytes?;
    // `ParquetBloomBlock`s of 32 bytes
    let data_start = offset + at as u64;
    if size == 0 || size % 32 != 0 || size as u64 > file_size || data_start > file_size - size as u64 {
        return None;
    }
    if let Some(length) = length
        && (at as u64 > length || size as u64 != length - at as u64)
    {
        return None;
    }
    let mut data = vec![0u8; size];
    file.seek(SeekFrom::Start(data_start)).ok()?;
    file.read_exact(&mut data).ok()?;
    Some(crate::execution::parquet_writer::BloomFilter { data })
}

/// `parquet_file_metadata`: the row of SQL of a file.
pub fn file_metadata_row(path: &str) -> Result<String> {
    let (metadata, size) = read_metadata(path)?;
    let file = metadata.file_metadata();
    // `ParquetFileMetadataCache::footer_size`: the length the file's last 8 bytes give the footer
    let footer_size = footer_length(path).unwrap_or(0);
    // `ConvertParquetElementToString` of each `ColumnOrder`, as Thrift prints it
    let column_orders = match file.column_orders() {
        Some(orders) => format!(
            "[{}]::VARCHAR[]",
            orders
                .iter()
                .map(|order| match order {
                    parquet::basic::ColumnOrder::TYPE_DEFINED_ORDER(_) => {
                        "'ColumnOrder(TYPE_ORDER=TypeDefinedOrder())'"
                    }
                    // the orders the C++ Thrift schema has no field for
                    _ => "'ColumnOrder(TYPE_ORDER=<null>)'",
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => "NULL::VARCHAR[]".to_string(),
    };
    Ok(format!(
        "({}, {}, {}, {}, {}, NULL, NULL, {size}::UBIGINT, {footer_size}::UBIGINT, {column_orders})",
        sql_text(path),
        sql_or_null(file.created_by().map(str::to_string)),
        file.num_rows(),
        metadata.num_row_groups(),
        file.version(),
    ))
}

/// The length of the footer of a Parquet file: the little-endian `u32` before the closing `PAR1`.
fn footer_length(path: &str) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::End(-8)).ok()?;
    let mut tail = [0u8; 8];
    file.read_exact(&mut tail).ok()?;
    Some(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as u64)
}

/// The part of `ParquetScanFunction::ProjectionExpressionPushdown` that reads the file: whether the column of the
/// scan's (only) file at `column` can be read as the byte lengths of its values. Its values are stored without
/// `DELTA_LENGTH_BYTE_ARRAY` in every row group.
pub fn reads_byte_length(scan: &ParquetScanData, column: usize) -> bool {
    let (Some(path), Some(name)) = (scan.files.first(), scan.names.get(column)) else {
        return false;
    };
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(metadata) = ParquetMetaDataReader::new().parse_and_finish(&file) else {
        return false;
    };
    let schema = metadata.file_metadata().schema_descr();
    let fields = schema.root_schema().get_fields();
    let Some(root) = fields.iter().position(|field| field.name().eq_ignore_ascii_case(name)) else {
        return false;
    };
    if !fields[root].is_primitive() {
        return false;
    }
    let Some(leaf) = (0..schema.num_columns()).find(|leaf| schema.get_column_root_idx(*leaf) == root) else {
        return false;
    };
    metadata.row_groups().iter().all(|group| {
        group
            .column(leaf)
            .encodings()
            .all(|encoding| encoding != parquet::basic::Encoding::DELTA_LENGTH_BYTE_ARRAY)
    })
}

/// The statistics of a VARIANT column of a Parquet file (Stage 4's port of `TransformColumnStatistics`), worked out
/// in the form of `VariantStatsShreddingState`, merged by `MergeShredding` and given to the catalog as its
/// [`crate::catalog::VariantStatistics`].
mod variant_stats {
    /// Of the `value` leaf of a shredding: whether it has NULLs, whether it has values, and its smallest and
    /// largest bytes.
    type LeafValues = (bool, bool, Option<(Vec<u8>, Vec<u8>)>);

    use super::*;
    use crate::optimizer::statistics::union;

    type Stats = crate::catalog::ColumnStatistics;

    fn unknown() -> Stats {
        Stats::unknown()
    }

    const VARIANT_INCONSISTENT: u8 = 3;

    /// `VariantStatsShreddingState` as the catalog keeps it.
    pub(super) fn to_catalog(state: Option<u8>, shredded: Shredding) -> Option<Box<crate::catalog::VariantStatistics>> {
        use crate::catalog::VariantStatistics as V;
        Some(Box::new(match (state?, shredded) {
            (0, _) => V::Uninitialized,
            (1, _) => V::NotShredded,
            (VARIANT_SHREDDED, Some(shredded)) => V::Shredded(shredded.0, shredded.1),
            _ => V::Inconsistent,
        }))
    }

    /// What [`to_catalog`] made of it, back as a state and a shredding.
    fn from_catalog(variant: Option<Box<crate::catalog::VariantStatistics>>) -> (Option<u8>, Shredding) {
        use crate::catalog::VariantStatistics as V;
        match variant.map(|variant| *variant) {
            None => (None, None),
            Some(V::Uninitialized) => (Some(0), None),
            Some(V::NotShredded) => (Some(1), None),
            Some(V::Shredded(ty, stats)) => (Some(VARIANT_SHREDDED), Some(Box::new((ty, stats)))),
            Some(V::Inconsistent) => (Some(VARIANT_INCONSISTENT), None),
        }
    }

    /// `VariantStats::Merge` of the statistics of two files.
    pub(super) fn merge_catalog(
        a: Option<Box<crate::catalog::VariantStatistics>>,
        b: Option<Box<crate::catalog::VariantStatistics>>,
    ) -> Option<Box<crate::catalog::VariantStatistics>> {
        let (a_state, a_shredded) = from_catalog(a);
        let (b_state, b_shredded) = from_catalog(b);
        let (state, shredded) = merge_variant_state(a_state, a_shredded, b_state, b_shredded);
        to_catalog(state, shredded)
    }

    /// A shredding: the type of a VARIANT's shredded part and its statistics.
    type Shredding = Option<Box<(LogicalType, Stats)>>;

    /// `VariantStats::Merge` of the shredding states and the shredded parts: UNINITIALIZED takes the other,
    /// two shredded alike are merged, and any others that differ are INCONSISTENT.
    fn merge_variant_state(
        a: Option<u8>,
        a_shredded: Shredding,
        b: Option<u8>,
        b_shredded: Shredding,
    ) -> (Option<u8>, Shredding) {
        const UNINITIALIZED: u8 = 0;
        const NOT_SHREDDED: u8 = 1;
        const SHREDDED: u8 = 2;
        const INCONSISTENT: u8 = 3;
        let (Some(a), Some(b)) = (a, b) else {
            return (None, None);
        };
        match (a, b) {
            (_, UNINITIALIZED) => (Some(a), a_shredded),
            (INCONSISTENT, _) => (Some(INCONSISTENT), None),
            (UNINITIALIZED, _) => (Some(b), b_shredded),
            (NOT_SHREDDED, NOT_SHREDDED) => (Some(NOT_SHREDDED), None),
            (SHREDDED, SHREDDED) => match (a_shredded, b_shredded) {
                (Some(x), Some(y)) => match merge_shredding(&x.0, &x.1, &y.0, &y.1) {
                    Some(merged) => (Some(SHREDDED), Some(Box::new(merged))),
                    None => (Some(INCONSISTENT), None),
                },
                _ => (Some(INCONSISTENT), None),
            },
            _ => (Some(INCONSISTENT), None),
        }
    }

    /// `VariantStats::GetTypedStats`: of `STRUCT(typed_value, ...)` its `typed_value`, else itself.
    fn typed_stats<'a>(ty: &'a LogicalType, stats: &'a Stats) -> Option<(&'a LogicalType, &'a Stats)> {
        if ty.id() != LogicalTypeId::Struct {
            return Some((ty, stats));
        }
        Some((&ty.struct_children().first()?.1, stats.children.first()?))
    }

    /// `VariantStats::GetUntypedStats`
    fn untyped_stats(ty: &LogicalType, stats: &Stats) -> Option<Stats> {
        (ty.id() == LogicalTypeId::Struct && ty.struct_children().len() == 2)
            .then(|| stats.children.get(1).cloned().unwrap_or_else(unknown))
    }

    /// `WrapTypedValue`: the shredding of these typed value statistics, with an `untyped_value_index`.
    fn wrap_typed_value(typed_type: LogicalType, typed: Stats, untyped: Option<Stats>) -> (LogicalType, Stats) {
        let nested = matches!(
            typed_type.id(),
            LogicalTypeId::Struct | LogicalTypeId::List | LogicalTypeId::Array
        );
        if untyped.is_none() && !nested {
            return (typed_type, typed);
        }
        let mut fields = vec![("typed_value".to_string(), typed_type)];
        let mut children = vec![typed];
        if let Some(untyped) = untyped {
            fields.push(("untyped_value_index".to_string(), LogicalType::UINTEGER));
            children.push(untyped);
        }
        // `BaseStatistics::CreateEmpty`
        let stats = Stats {
            can_be_null: false,
            can_be_valid: false,
            children,
            ..unknown()
        };
        (LogicalType::struct_type(fields), stats)
    }

    /// `VariantStats::MergeShredding`: of two shreddings the fields they share as alike, `None` if nothing is.
    fn merge_shredding(a_ty: &LogicalType, a: &Stats, b_ty: &LogicalType, b: &Stats) -> Option<(LogicalType, Stats)> {
        let (a_type, a_typed) = typed_stats(a_ty, a)?;
        let (b_type, b_typed) = typed_stats(b_ty, b)?;
        let untyped = match (untyped_stats(a_ty, a), untyped_stats(b_ty, b)) {
            (Some(x), Some(y)) => Some(union(x, y)),
            (x, y) => x.or(y),
        };
        // `CombineValidity`
        let validity = |children: Vec<Stats>| Stats {
            can_be_null: a_typed.can_be_null || b_typed.can_be_null,
            can_be_valid: a_typed.can_be_valid || b_typed.can_be_valid,
            children,
            ..unknown()
        };
        match a_type.id() {
            LogicalTypeId::Struct => {
                if b_type.id() != LogicalTypeId::Struct {
                    return None;
                }
                let b_fields = b_type.struct_children();
                let mut fields = Vec::new();
                let mut children = Vec::new();
                for (at, (name, field)) in a_type.struct_children().iter().enumerate() {
                    let Some(other_at) = b_fields.iter().position(|(other, _)| other == name) else {
                        continue;
                    };
                    let other = &b_fields[other_at].1;
                    if other.id() != field.id() {
                        continue;
                    }
                    let (Some(x), Some(y)) = (a_typed.children.get(at), b_typed.children.get(other_at)) else {
                        continue;
                    };
                    if let Some((ty, stats)) = merge_shredding(field, x, other, y) {
                        fields.push((name.clone(), ty));
                        children.push(stats);
                    }
                }
                if fields.is_empty() {
                    return None;
                }
                Some(wrap_typed_value(
                    LogicalType::struct_type(fields),
                    validity(children),
                    untyped,
                ))
            }
            LogicalTypeId::List => {
                if b_type.id() != LogicalTypeId::List {
                    return None;
                }
                let (x, y) = (a_typed.children.first()?, b_typed.children.first()?);
                let (child_type, child) = merge_shredding(a_type.child_type()?, x, b_type.child_type()?, y)?;
                Some(wrap_typed_value(
                    LogicalType::list(child_type),
                    validity(vec![child]),
                    untyped,
                ))
            }
            _ => {
                if a_type != b_type {
                    return None;
                }
                Some(wrap_typed_value(
                    a_type.clone(),
                    union(a_typed.clone(), b_typed.clone()),
                    untyped,
                ))
            }
        }
    }

    const VARIANT_SHREDDED: u8 = 2;

    /// `TransformColumnStatistics` of a VARIANT group (`metadata`, `value`, `typed_value`): per row group, the shredding
    /// of its `typed_value` with the statistics of its parts, merged across the row groups (`MergeShredding`). Parts
    /// whose statistics can't be told are left unknown, which makes them not fully shredded, rather than the whole
    /// column's statistics going. With `only`, those of that row group alone (`ColumnReader::Stats`).
    pub(super) fn variant_statistics(
        metadata: &ParquetMetaData,
        name: &str,
        only: Option<usize>,
    ) -> Option<crate::catalog::ColumnStatistics> {
        let descriptor = metadata.file_metadata().schema_descr();
        let field = descriptor
            .root_schema()
            .get_fields()
            .iter()
            .find(|field| field.name().eq_ignore_ascii_case(name))?;
        let path = vec![field.name().to_string()];
        let fields = (!field.is_primitive()).then(|| field.get_fields())?;
        if fields.len() != 3
            || fields[0].name() != "metadata"
            || fields[1].name() != "value"
            || fields[2].name() != "typed_value"
        {
            return None;
        }
        // the types the leaves read as
        let arrow = parquet::arrow::parquet_to_arrow_schema(descriptor, None).ok()?;
        let arrow_field = arrow.fields().iter().find(|f| f.name() == field.name())?;
        let DataType::Struct(arrow_parts) = arrow_field.data_type() else {
            return None;
        };
        let arrow_typed = arrow_parts.iter().find(|f| f.name() == "typed_value")?;
        let leaves: Vec<Vec<String>> = (0..descriptor.num_columns())
            .map(|i| descriptor.column(i).path().parts().to_vec())
            .collect();
        let mut merged: Option<(Option<u8>, Shredding)> = None;
        for (at, group) in metadata.row_groups().iter().enumerate() {
            if only.is_some_and(|only| only != at) {
                continue;
            }
            let leaf_stats = |path: &[String], ty: &LogicalType| -> Option<crate::catalog::ColumnStatistics> {
                let leaf = leaves.iter().position(|leaf| leaf == path)?;
                let zone = leaf_zone(metadata, group, leaf, ty, true)?;
                let statistics = group.column(leaf).statistics()?;
                let all_null = statistics.null_count_opt()? >= group.column(leaf).num_values() as u64;
                // `TransformParquetStatistics` of strings: their bounds as the file has them
                let strings = matches!(ty.id(), LogicalTypeId::Varchar | LogicalTypeId::Blob).then(|| {
                    let exact = statistics.min_is_exact() && statistics.max_is_exact();
                    let bounds = statistics
                        .min_bytes_opt()
                        .zip(statistics.max_bytes_opt())
                        .filter(|_| exact && !all_null)
                        .filter(|(min, max)| {
                            ty.id() == LogicalTypeId::Blob
                                || (std::str::from_utf8(min).is_ok() && std::str::from_utf8(max).is_ok())
                        })
                        .map(|(min, max)| (min.to_vec(), max.to_vec()));
                    crate::catalog::StringStatistics {
                        bounds,
                        has_unicode: true,
                        min_length: None,
                        max_length: None,
                        total_length: None,
                    }
                });
                Some(crate::catalog::ColumnStatistics {
                    can_be_null: zone.has_null,
                    can_be_valid: !all_null,
                    min: zone.min.filter(|_| !zone.unknown),
                    max: zone.max.filter(|_| !zone.unknown),
                    strings,
                    ..unknown()
                })
            };
            let value_leaf = |path: &[String]| -> Option<LeafValues> {
                let leaf = leaves.iter().position(|leaf| leaf == path)?;
                let statistics = group.column(leaf).statistics()?;
                let nulls = statistics.null_count_opt()?;
                let bounds = statistics
                    .min_bytes_opt()
                    .zip(statistics.max_bytes_opt())
                    .map(|(min, max)| (min.to_vec(), max.to_vec()));
                Some((nulls > 0, nulls < group.column(leaf).num_values() as u64, bounds))
            };
            let shredding = variant_node_stats(&path, &fields[1], &fields[2], arrow_typed, &leaf_stats, &value_leaf)?;
            let state = (Some(VARIANT_SHREDDED), Some(Box::new(shredding)));
            merged = Some(match merged {
                None => state,
                Some((state_a, shredded_a)) => merge_variant_state(state_a, shredded_a, state.0, state.1),
            });
        }
        let (variant_state, variant_shredded) = merged?;
        Some(crate::catalog::ColumnStatistics {
            variant: to_catalog(variant_state, variant_shredded),
            // `SetHasNoNull` and `SetHasNull`: the validity of the column is not known
            can_be_null: true,
            can_be_valid: true,
            ..unknown()
        })
    }

    /// `ConvertShreddedStatsItem`: the shredding of a part of a VARIANT, `STRUCT(typed_value, untyped_value_index)`,
    /// and its statistics, of its `value` and `typed_value` at `path`.
    fn variant_node_stats(
        path: &[String],
        value: &parquet::schema::types::Type,
        typed_value: &parquet::schema::types::Type,
        arrow: &Field,
        leaf_stats: &dyn Fn(&[String], &LogicalType) -> Option<crate::catalog::ColumnStatistics>,
        value_leaf: &dyn Fn(&[String]) -> Option<LeafValues>,
    ) -> Option<(LogicalType, crate::catalog::ColumnStatistics)> {
        let child = |name: &str| {
            let mut path = path.to_vec();
            path.push(name.to_string());
            path
        };
        // `ConvertUnshreddedStats`: of the values that are not shredded, NULLs or VARIANT NULLs (a lone 0 byte) alone
        // leave the part fully shredded
        let untyped = match value_leaf(&child(value.name())) {
            None => unknown(),
            Some((has_null, has_value, bounds)) => {
                let mut stats = crate::catalog::ColumnStatistics {
                    can_be_null: has_null,
                    can_be_valid: has_value,
                    ..unknown()
                };
                if has_value {
                    match bounds {
                        Some((min, max)) if min == [0] && max == [0] => {
                            stats.min = Some(Value::uinteger(0));
                            stats.max = Some(Value::uinteger(0));
                        }
                        Some(_) => {}
                        None => stats = unknown(),
                    }
                }
                stats
            }
        };
        let (typed_type, typed) =
            variant_typed_stats(&child(typed_value.name()), typed_value, arrow, leaf_stats, value_leaf)?;
        let ty = LogicalType::struct_type(vec![
            ("typed_value".to_string(), typed_type),
            ("untyped_value_index".to_string(), LogicalType::UINTEGER),
        ]);
        // `BaseStatistics::CreateEmpty` of the part itself
        let stats = crate::catalog::ColumnStatistics {
            can_be_null: false,
            can_be_valid: false,
            children: vec![typed, untyped],
            ..unknown()
        };
        Some((ty, stats))
    }

    /// `ConvertShreddedStats` of a `typed_value` at `path`: of a leaf its statistics, of a LIST those of its elements,
    /// of a group those of its fields (`VariantColumnReader::TypedValueLayoutToType` for the type).
    fn variant_typed_stats(
        path: &[String],
        typed_value: &parquet::schema::types::Type,
        arrow: &Field,
        leaf_stats: &dyn Fn(&[String], &LogicalType) -> Option<crate::catalog::ColumnStatistics>,
        value_leaf: &dyn Fn(&[String]) -> Option<LeafValues>,
    ) -> Option<(LogicalType, crate::catalog::ColumnStatistics)> {
        let child = |name: &str| {
            let mut path = path.to_vec();
            path.push(name.to_string());
            path
        };
        // the `typed_value` and `value` of a part, as the file and as Arrow have them
        let parts = |group: &parquet::schema::types::Type, arrow: &Field| -> Option<(_, _, Field)> {
            let parts = group.get_fields();
            let value = parts.iter().find(|part| part.name() == "value")?.clone();
            let typed = parts.iter().find(|part| part.name() == "typed_value")?.clone();
            let DataType::Struct(arrow_parts) = arrow.data_type() else {
                return None;
            };
            let arrow_typed = arrow_parts.iter().find(|f| f.name() == "typed_value")?.as_ref().clone();
            Some((value, typed, arrow_typed))
        };
        if typed_value.is_primitive() {
            let ty = field_type(arrow).ok()?;
            let stats = leaf_stats(path, &ty).unwrap_or_else(unknown);
            return Some((ty, stats));
        }
        let fields = typed_value.get_fields();
        let is_list = matches!(
            typed_value.get_basic_info().logical_type_ref(),
            Some(parquet::basic::LogicalType::List)
        ) || typed_value.get_basic_info().converted_type() == parquet::basic::ConvertedType::LIST;
        if is_list {
            // three levels: the LIST, its repeated group, and the element with its `value` and `typed_value`
            let repeated = fields.first()?;
            let element = repeated.get_fields().first()?;
            let element_path = {
                let mut path = child(repeated.name());
                path.push(element.name().to_string());
                path
            };
            let (DataType::List(arrow_element) | DataType::LargeList(arrow_element)) = arrow.data_type() else {
                return None;
            };
            let (value, typed, arrow_typed) = parts(element, arrow_element)?;
            let (element_type, element_stats) =
                variant_node_stats(&element_path, &value, &typed, &arrow_typed, leaf_stats, value_leaf)?;
            let stats = crate::catalog::ColumnStatistics {
                children: vec![element_stats],
                ..unknown()
            };
            return Some((LogicalType::list(element_type), stats));
        }
        // an object: a group of fields, each with its `value` and `typed_value`
        let mut types = Vec::with_capacity(fields.len());
        let mut children = Vec::with_capacity(fields.len());
        let DataType::Struct(arrow_fields) = arrow.data_type() else {
            return None;
        };
        for field in fields {
            let arrow_field = arrow_fields.iter().find(|f| f.name() == field.name())?;
            let (value, typed, arrow_typed) = parts(field, arrow_field)?;
            let (field_type, field_stats) = variant_node_stats(
                &child(field.name()),
                &value,
                &typed,
                &arrow_typed,
                leaf_stats,
                value_leaf,
            )?;
            types.push((field.name().to_string(), field_type));
            children.push(field_stats);
        }
        let stats = crate::catalog::ColumnStatistics { children, ..unknown() };
        Some((LogicalType::struct_type(types), stats))
    }

    /// What the statistics of row group `group` say of the leaf `leaf`, read as a `ty`. With `float_bounds`, the bounds
    /// of floats are taken as `TransformParquetStatistics` takes them: as they are, but for a maximum of NaN where the
    /// file says there are NaNs (`CreateFloatingPointStats`).
    fn leaf_zone(
        metadata: &ParquetMetaData,
        group: &parquet::file::metadata::RowGroupMetaData,
        leaf: usize,
        ty: &LogicalType,
        float_bounds: bool,
    ) -> Option<crate::catalog::ColumnZone> {
        use parquet::basic::Type as Physical;
        let descriptor = metadata.file_metadata().schema_descr().column(leaf);
        let statistics = group.column(leaf).statistics()?;
        // (of a leaf in a list, its values rather than the rows)
        let rows = group.column(leaf).num_values() as u64;
        let nulls = statistics.null_count_opt()?;
        let mut zone = crate::catalog::ColumnZone {
            has_null: nulls > 0,
            ..crate::catalog::ColumnZone::default()
        };
        if nulls >= rows {
            return Some(zone);
        }
        // NaNs are left out of float bounds, but are the largest values to DuckDB; INT96 bounds compare bytes
        let float = matches!(descriptor.physical_type(), Physical::FLOAT | Physical::DOUBLE)
            || matches!(ty.id(), LogicalTypeId::Float | LogicalTypeId::Double);
        let bounded = descriptor.physical_type() != Physical::INT96 && (!float || float_bounds);
        // byte arrays with only the deprecated bounds may be ordered as signed bytes
        let deprecated = statistics.is_min_max_deprecated()
            && matches!(
                descriptor.physical_type(),
                Physical::BYTE_ARRAY | Physical::FIXED_LEN_BYTE_ARRAY
            );
        let bound = |bytes: Option<&[u8]>, exact: bool| -> Option<Value> {
            let bytes = bytes?;
            // `StringStatsAreValid`: exact bounds of a VARCHAR that are not UTF-8 are bogus
            if ty.id() == LogicalTypeId::Varchar && std::str::from_utf8(bytes).is_err() {
                return None;
            }
            let text = statistic_text(&descriptor, bytes)?;
            if !exact || deprecated {
                return None;
            }
            try_cast_value(&Value::varchar(text), ty)
        };
        let min = bound(statistics.min_bytes_opt(), statistics.min_is_exact());
        let max = bound(statistics.max_bytes_opt(), statistics.max_is_exact());
        let max = match max {
            Some(_) if float && statistics.nan_count_opt().is_some_and(|nans| nans > 0) => {
                try_cast_value(&Value::varchar("nan"), ty)
            }
            max => max,
        };
        match (min, max) {
            (Some(min), Some(max)) if bounded => {
                zone.min = Some(min);
                zone.max = Some(max);
            }
            _ => zone.unknown = true,
        }
        Some(zone)
    }
}
