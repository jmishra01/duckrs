// Ported from duckdb/extension/parquet/parquet_writer.cpp, column_writer.cpp, writer/*.cpp and
// include/writer/{templated_column_writer,parquet_write_stats,parquet_write_operators}.hpp @ rust-port-baseline
//
//! DuckDB's own Parquet writer (`ParquetWriter`), so that files come out as C++ writes them: its choice of encodings,
//! dictionaries, pages, statistics and bloom filters, and its footer. A row group is written in the passes of
//! `PrepareRowGroup`: analyze (build the dictionaries), prepare (levels and pages), write, and finalize.
//!
//! Values come in as [`Value`]s, one column at a time, in vectors of [`STANDARD_VECTOR_SIZE`] rows as
//! `ColumnDataCollection` hands them out, because where pages end depends on that.

mod encoding;

pub use encoding::BloomFilter;

use duckrs_common::types::{LogicalType, LogicalTypeId, TimeTz};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use encoding::{
    BssEncoder, DbpEncoder, DlbaEncoder, RleBpEncoder, Thrift, bit_width_of_count, bit_width_of_max, ttype, xxhash64,
};
use std::collections::HashMap;
use std::io::Write;

/// What `CREATED_BY` says (`DuckDB version %s (build %s)`).
const CREATED_BY: &str = "DuckDB version v2.0.0 (build d4eea3f208d)";
/// `DEFAULT_ROW_GROUP_SIZE`.
pub const DEFAULT_ROW_GROUP_SIZE: usize = 122_880;
/// `PrimitiveColumnWriter::MAX_UNCOMPRESSED_PAGE_SIZE`: without a page index, one page per column chunk.
pub const MAX_UNCOMPRESSED_PAGE_SIZE: usize = 104_857_600;
/// `PrimitiveColumnWriter::MAX_UNCOMPRESSED_DICT_PAGE_SIZE`.
pub const MAX_UNCOMPRESSED_DICT_PAGE_SIZE: usize = 1_073_741_824;
/// `PrimitiveColumnWriter::MIN_COMPRESSED_TO_FILTER_RATIO`.
const MIN_COMPRESSED_TO_FILTER_RATIO: usize = 2;
/// `MAX_STRING_STATISTICS_SIZE`.
const MAX_STRING_STATISTICS_SIZE: usize = 256;
/// `ColumnWriter::PARQUET_DEFINE_VALID`.
const DEFINE_VALID: u16 = u16::MAX;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// `duckdb_parquet::Type`.
mod physical {
    pub const BOOLEAN: i32 = 0;
    pub const INT32: i32 = 1;
    pub const INT64: i32 = 2;
    pub const INT96: i32 = 3;
    pub const FLOAT: i32 = 4;
    pub const DOUBLE: i32 = 5;
    pub const BYTE_ARRAY: i32 = 6;
    pub const FIXED_LEN_BYTE_ARRAY: i32 = 7;
}

/// `duckdb_parquet::Encoding`.
mod enc {
    pub const PLAIN: i32 = 0;
    pub const PLAIN_DICTIONARY: i32 = 2;
    pub const RLE: i32 = 3;
    pub const DELTA_BINARY_PACKED: i32 = 5;
    pub const DELTA_LENGTH_BYTE_ARRAY: i32 = 6;
    pub const RLE_DICTIONARY: i32 = 8;
    pub const BYTE_STREAM_SPLIT: i32 = 9;
}

/// `duckdb_parquet::ConvertedType`.
mod converted {
    pub const UTF8: i32 = 0;
    pub const MAP: i32 = 1;
    pub const LIST: i32 = 3;
    pub const DECIMAL: i32 = 5;
    pub const DATE: i32 = 6;
    pub const TIME_MICROS: i32 = 8;
    pub const TIMESTAMP_MILLIS: i32 = 9;
    pub const TIMESTAMP_MICROS: i32 = 10;
    pub const UINT_8: i32 = 11;
    pub const UINT_16: i32 = 12;
    pub const UINT_32: i32 = 13;
    pub const UINT_64: i32 = 14;
    pub const INT_8: i32 = 15;
    pub const INT_16: i32 = 16;
    pub const INT_32: i32 = 17;
    pub const INT_64: i32 = 18;
    pub const JSON: i32 = 19;
    pub const INTERVAL: i32 = 21;
}

const REQUIRED: i32 = 0;
const OPTIONAL: i32 = 1;
const REPEATED: i32 = 2;
const DATA_PAGE: i32 = 0;
const DICTIONARY_PAGE: i32 = 2;

/// `duckdb_parquet::CompressionCodec`, with what it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Uncompressed,
    Snappy,
    Gzip,
    Brotli,
    Zstd(i32),
    Lz4Raw,
}

impl Codec {
    fn thrift(self) -> i32 {
        match self {
            Codec::Uncompressed => 0,
            Codec::Snappy => 1,
            Codec::Gzip => 2,
            Codec::Brotli => 4,
            Codec::Zstd(_) => 6,
            Codec::Lz4Raw => 7,
        }
    }

    /// `ColumnWriter::CompressPage`.
    fn compress(self, data: Vec<u8>) -> Result<Vec<u8>> {
        let failed = |e: std::io::Error| Error::new(ErrorKind::Io, format!("Could not compress a Parquet page: {e}"));
        Ok(match self {
            Codec::Uncompressed => data,
            Codec::Snappy => snap::raw::Encoder::new()
                .compress_vec(&data)
                .map_err(|e| Error::new(ErrorKind::Io, format!("Could not compress a Parquet page: {e}")))?,
            Codec::Gzip => {
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&data).map_err(failed)?;
                encoder.finish().map_err(failed)?
            }
            Codec::Brotli => {
                let mut out = Vec::new();
                let params = brotli::enc::BrotliEncoderParams {
                    quality: 11,
                    lgwin: 22,
                    ..Default::default()
                };
                brotli::BrotliCompress(&mut data.as_slice(), &mut out, &params).map_err(failed)?;
                out
            }
            Codec::Zstd(level) => zstd::bulk::compress(&data, level).map_err(failed)?,
            Codec::Lz4Raw => lz4_flex::block::compress(&data),
        })
    }
}

/// What COPY ... (FORMAT PARQUET) says of the file (`ParquetWriterOptions`).
#[derive(Debug, Clone)]
pub struct WriterOptions {
    pub codec: Codec,
    /// 1 or 2
    pub version: i32,
    pub row_group_size: usize,
    pub row_group_size_bytes: Option<usize>,
    pub dictionary_size_limit: Option<usize>,
    pub string_dictionary_page_size_limit: usize,
    pub data_page_size_limit: usize,
    pub bloom_filters: bool,
    pub bloom_filter_fpp: f64,
    pub key_values: Vec<(String, Vec<u8>)>,
    pub timestamp_as_int96: bool,
    /// `bloom_filter_fallback_max_bytes`: what the bloom filters of columns without a dictionary may take
    pub bloom_filter_fallback_max_bytes: usize,
    /// `not_null_columns`: the columns written REQUIRED, by position
    pub not_null_columns: Vec<bool>,
    /// `write_page_index`: a ColumnIndex and an OffsetIndex for each column chunk of more than one page
    pub write_page_index: bool,
    /// `FIELD_IDS`
    pub field_ids: FieldIds,
}

/// `ChildFieldIDs`: the field ids of columns by name, each with those of the columns in it.
#[derive(Debug, Clone, Default)]
pub struct FieldIds(pub HashMap<String, (Option<i32>, FieldIds)>);

impl FieldIds {
    fn get(&self, name: &str) -> Option<&(Option<i32>, FieldIds)> {
        self.0.get(&name.to_lowercase())
    }
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            codec: Codec::Snappy,
            version: 1,
            row_group_size: DEFAULT_ROW_GROUP_SIZE,
            row_group_size_bytes: None,
            dictionary_size_limit: None,
            string_dictionary_page_size_limit: MAX_UNCOMPRESSED_DICT_PAGE_SIZE,
            data_page_size_limit: MAX_UNCOMPRESSED_PAGE_SIZE,
            bloom_filters: true,
            bloom_filter_fpp: 0.01,
            key_values: Vec::new(),
            timestamp_as_int96: false,
            bloom_filter_fallback_max_bytes: 64 * 1024 * 1024,
            not_null_columns: Vec::new(),
            write_page_index: false,
            field_ids: FieldIds::default(),
        }
    }
}

fn io_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Io, message.into())
}

// ---------------------------------------------------------------------------------------------------------------
// Values as Parquet stores them
// ---------------------------------------------------------------------------------------------------------------

/// A value converted to what the file holds (the `TGT` of the C++ templates).
#[derive(Debug, Clone, PartialEq)]
enum Target {
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F32(f32),
    F64(f64),
    Bytes(Vec<u8>),
    Fixed(Vec<u8>),
}

impl Target {
    /// The PLAIN encoding (`WriteToStream`).
    fn plain(&self, out: &mut Vec<u8>) {
        match self {
            Target::I32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Target::U32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Target::I64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Target::U64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Target::F32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Target::F64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Target::Bytes(v) => {
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                out.extend_from_slice(v);
            }
            Target::Fixed(v) => out.extend_from_slice(v),
        }
    }

    /// The bytes of a statistic, and of what bloom filters hash: PLAIN without the length of byte arrays.
    fn raw(&self) -> Vec<u8> {
        match self {
            Target::Bytes(v) | Target::Fixed(v) => v.clone(),
            other => {
                let mut out = Vec::new();
                other.plain(&mut out);
                out
            }
        }
    }

    fn plain_size(&self) -> usize {
        match self {
            Target::I32(_) | Target::U32(_) | Target::F32(_) => 4,
            Target::I64(_) | Target::U64(_) | Target::F64(_) => 8,
            Target::Bytes(v) => 4 + v.len(),
            Target::Fixed(v) => v.len(),
        }
    }

    /// What dictionaries tell values apart by: NaNs are one value, and so are the zeros (`float_na_equal`).
    fn key(&self) -> Vec<u8> {
        match self {
            Target::F32(v) if v.is_nan() => f32::NAN.to_le_bytes().to_vec(),
            Target::F32(v) if *v == 0.0 => 0f32.to_le_bytes().to_vec(),
            Target::F64(v) if v.is_nan() => f64::NAN.to_le_bytes().to_vec(),
            Target::F64(v) if *v == 0.0 => 0f64.to_le_bytes().to_vec(),
            other => {
                let mut out = Vec::new();
                other.plain(&mut out);
                out
            }
        }
    }

    fn as_i64(&self) -> i64 {
        match self {
            Target::I32(v) => i64::from(*v),
            Target::U32(v) => i64::from(*v),
            Target::I64(v) => *v,
            Target::U64(v) => *v as i64,
            _ => 0,
        }
    }

    /// `LessThan` within one kind of target.
    fn less(&self, other: &Target) -> bool {
        match (self, other) {
            (Target::I32(a), Target::I32(b)) => a < b,
            (Target::U32(a), Target::U32(b)) => a < b,
            (Target::I64(a), Target::I64(b)) => a < b,
            (Target::U64(a), Target::U64(b)) => a < b,
            (Target::F32(a), Target::F32(b)) => a < b,
            (Target::F64(a), Target::F64(b)) => a < b,
            (Target::Bytes(a), Target::Bytes(b)) | (Target::Fixed(a), Target::Fixed(b)) => a < b,
            _ => false,
        }
    }
}

/// How a DuckDB value becomes a [`Target`] (the operators of `parquet_write_operators.hpp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Convert {
    I32,
    U32,
    I64,
    U64,
    /// `ParquetTimestampSOperator`: seconds to microseconds
    TimestampSec,
    /// `ParquetTimestamp*Int96Operator`, from microseconds after scaling by the factor
    Int96 {
        divide: i64,
        multiply: i64,
    },
    /// `ParquetTimeTZOperator`: the time in UTC
    TimeTz,
    /// `ParquetHugeintOperator`
    Double,
    F32,
    F64,
    Bytes,
    /// a type Parquet doesn't have, written as its text (`IsTypeNotSupported`)
    Text,
    Uuid,
    Interval,
}

impl Convert {
    fn physical(self) -> i32 {
        match self {
            Convert::I32 | Convert::U32 => physical::INT32,
            Convert::I64 | Convert::U64 | Convert::TimestampSec | Convert::TimeTz => physical::INT64,
            Convert::Int96 { .. } => physical::INT96,
            Convert::F32 => physical::FLOAT,
            Convert::F64 | Convert::Double => physical::DOUBLE,
            Convert::Bytes | Convert::Text => physical::BYTE_ARRAY,
            Convert::Uuid | Convert::Interval => physical::FIXED_LEN_BYTE_ARRAY,
        }
    }

    fn apply(self, value: &Value) -> Result<Target> {
        let int = || value.as_i128().unwrap_or(0);
        Ok(match self {
            Convert::I32 => Target::I32(int() as i32),
            Convert::U32 => Target::U32(int() as u32),
            Convert::I64 => Target::I64(int() as i64),
            Convert::U64 => Target::U64(int() as u64),
            Convert::TimestampSec => {
                let seconds = int() as i64;
                if seconds == i64::MAX || seconds == -i64::MAX {
                    Target::I64(seconds)
                } else {
                    Target::I64(
                        seconds
                            .checked_mul(1_000_000)
                            .ok_or_else(|| Error::new(ErrorKind::Conversion, "Timestamp out of range"))?,
                    )
                }
            }
            Convert::Int96 { divide, multiply } => {
                let raw = int() as i64;
                let micros = raw.div_euclid(divide).saturating_mul(multiply);
                // `TimestampToImpalaTimestamp`: nanoseconds of the day, then the Julian day
                let days = micros.div_euclid(MICROS_PER_DAY);
                let nanos = micros.rem_euclid(MICROS_PER_DAY) * 1000;
                let mut bytes = nanos.to_le_bytes().to_vec();
                bytes.extend_from_slice(&((days + 2_440_588) as u32).to_le_bytes());
                Target::Fixed(bytes)
            }
            Convert::TimeTz => {
                let bits = match value.data() {
                    ValueData::UInt64(bits) => *bits,
                    ValueData::Int64(bits) => *bits as u64,
                    _ => return Err(io_error("a TIMETZ value without its bits")),
                };
                let tz = TimeTz(bits);
                let utc = (tz.time().0 - i64::from(tz.offset()) * 1_000_000).rem_euclid(MICROS_PER_DAY);
                Target::I64(utc)
            }
            Convert::Double => Target::F64(match value.data() {
                ValueData::UInt128(v) => *v as f64,
                _ => int() as f64,
            }),
            Convert::F32 => Target::F32(match value.data() {
                ValueData::Float(v) => *v,
                _ => 0.0,
            }),
            Convert::F64 => Target::F64(match value.data() {
                ValueData::Double(v) => *v,
                _ => 0.0,
            }),
            Convert::Bytes => Target::Bytes(match value.data() {
                ValueData::Bytes(bytes) => bytes.clone(),
                _ => value.try_to_string()?.into_bytes(),
            }),
            Convert::Text => Target::Bytes(value.try_to_string()?.into_bytes()),
            Convert::Uuid => {
                let stored = int();
                Target::Fixed(((stored as u128) ^ (1 << 127)).to_be_bytes().to_vec())
            }
            Convert::Interval => {
                let ValueData::Interval(interval) = value.data() else {
                    return Err(io_error("an INTERVAL value without its parts"));
                };
                if interval.days < 0 || interval.months < 0 || interval.micros < 0 {
                    return Err(io_error("Parquet files do not support negative intervals"));
                }
                // `ParquetIntervalUtils::Encode`
                let (mut days, mut micros) = (i64::from(interval.days), interval.micros);
                if micros / 1000 > i64::from(u32::MAX) {
                    days += micros / MICROS_PER_DAY;
                    micros %= MICROS_PER_DAY;
                }
                if days > i64::from(i32::MAX) {
                    return Err(Error::new(
                        ErrorKind::OutOfRange,
                        format!(
                            "Cannot write the interval {} to Parquet, as its days plus the whole days of its time \
                             exceed {}",
                            value.try_to_string()?,
                            i32::MAX
                        ),
                    ));
                }
                let mut bytes = (interval.months as u32).to_le_bytes().to_vec();
                bytes.extend_from_slice(&(days as u32).to_le_bytes());
                bytes.extend_from_slice(&((micros / 1000) as u32).to_le_bytes());
                Target::Fixed(bytes)
            }
        })
    }
}

/// Which statistics a column keeps (`InitializeStats`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatsKind {
    /// none at all, not even the NULL count (`BaseParquetOperator`: intervals, INT96)
    Absent,
    /// the NULL count only (`ColumnWriterStatistics`: HUGEINT)
    Empty,
    /// `NumericStatisticsState`; `signed` writes the deprecated min and max as well
    Numeric {
        signed: bool,
    },
    Float,
    String {
        blob: bool,
    },
    Uuid,
}

/// The statistics of a column chunk, or of a page (`ColumnWriterStatistics`).
#[derive(Debug, Clone)]
enum Stats {
    Empty,
    Numeric {
        min: Option<Target>,
        max: Option<Target>,
        signed: bool,
    },
    Float {
        min: Option<Target>,
        max: Option<Target>,
        nan_count: u64,
    },
    String(StringStats),
    Uuid {
        min: Option<Vec<u8>>,
        max: Option<Vec<u8>>,
    },
    Bool {
        min: bool,
        max: bool,
    },
    Decimal {
        min: Option<i128>,
        max: Option<i128>,
    },
}

#[derive(Debug, Clone)]
struct StringStats {
    blob: bool,
    /// ENUM: the dictionary has values that no row has, so the bounds are not exact
    inexact: bool,
    has_stats: bool,
    failed_truncate: bool,
    min: Vec<u8>,
    max: Vec<u8>,
    min_truncated: bool,
    max_truncated: bool,
}

impl StringStats {
    fn new(blob: bool, inexact: bool) -> Self {
        Self {
            blob,
            inexact,
            has_stats: false,
            failed_truncate: false,
            min: Vec::new(),
            max: Vec::new(),
            min_truncated: false,
            max_truncated: false,
        }
    }

    /// `StringStatisticsState::Update`.
    fn update(&mut self, value: &[u8]) {
        if self.failed_truncate {
            return;
        }
        if !self.has_stats || value < self.min.as_slice() {
            if value.len() > MAX_STRING_STATISTICS_SIZE {
                self.min = self.truncate_min(value);
                self.min_truncated = true;
            } else {
                self.min = value.to_vec();
                self.min_truncated = false;
            }
        }
        if !self.has_stats || value > self.max.as_slice() {
            if value.len() > MAX_STRING_STATISTICS_SIZE {
                match self.truncate_max(value) {
                    Some(max) => {
                        self.max = max;
                        self.max_truncated = true;
                    }
                    None => {
                        self.failed_truncate = true;
                        self.has_stats = false;
                        self.min.clear();
                        self.max.clear();
                        return;
                    }
                }
            } else {
                self.max = value.to_vec();
                self.max_truncated = false;
            }
        }
        self.has_stats = true;
    }

    /// `TruncateMin`: a prefix, ending on a whole character of text.
    fn truncate_min(&self, value: &[u8]) -> Vec<u8> {
        let mut size = MAX_STRING_STATISTICS_SIZE;
        if !self.blob {
            while size > 0 && (value[size] & 0xc0) == 0x80 {
                size -= 1;
            }
        }
        value[..size].to_vec()
    }

    /// `TryTruncateMax`: a prefix whose last byte that can grow grows.
    fn truncate_max(&self, value: &[u8]) -> Option<Vec<u8>> {
        let max_byte = if self.blob { 0xff } else { 0x7f };
        let mut position = MAX_STRING_STATISTICS_SIZE;
        while position > 0 && value[position - 1] >= max_byte {
            position -= 1;
        }
        if position == 0 {
            return None;
        }
        let mut result = value[..position].to_vec();
        result[position - 1] += 1;
        Some(result)
    }
}

impl Stats {
    fn new(kind: StatsKind) -> Option<Self> {
        Some(match kind {
            StatsKind::Absent => return None,
            StatsKind::Empty => Stats::Empty,
            StatsKind::Numeric { signed } => Stats::Numeric {
                min: None,
                max: None,
                signed,
            },
            StatsKind::Float => Stats::Float {
                min: None,
                max: None,
                nan_count: 0,
            },
            StatsKind::String { blob } => Stats::String(StringStats::new(blob, false)),
            StatsKind::Uuid => Stats::Uuid { min: None, max: None },
        })
    }

    /// `HandleStats` of a written value.
    fn update(&mut self, value: &Target) {
        match self {
            Stats::Empty | Stats::Bool { .. } | Stats::Decimal { .. } => {}
            Stats::Numeric { min, max, .. } => {
                if min.as_ref().is_none_or(|m| value.less(m)) {
                    *min = Some(value.clone());
                }
                if max.as_ref().is_none_or(|m| m.less(value)) {
                    *max = Some(value.clone());
                }
            }
            Stats::Float { min, max, nan_count } => {
                let nan =
                    matches!(value, Target::F32(v) if v.is_nan()) || matches!(value, Target::F64(v) if v.is_nan());
                if nan {
                    *nan_count += 1;
                    return;
                }
                if min.as_ref().is_none_or(|m| value.less(m)) {
                    *min = Some(value.clone());
                }
                if max.as_ref().is_none_or(|m| m.less(value)) {
                    *max = Some(value.clone());
                }
            }
            Stats::String(stats) => {
                if let Target::Bytes(bytes) = value {
                    stats.update(bytes);
                }
            }
            Stats::Uuid { min, max } => {
                let bytes = value.raw();
                if min.as_ref().is_none_or(|m| bytes < *m) {
                    *min = Some(bytes.clone());
                }
                if max.as_ref().is_none_or(|m| bytes > *m) {
                    *max = Some(bytes);
                }
            }
        }
    }

    fn has_stats(&self) -> bool {
        match self {
            Stats::Empty => false,
            Stats::Numeric { min, .. } | Stats::Float { min, .. } => min.is_some(),
            Stats::String(stats) => stats.has_stats,
            Stats::Uuid { min, .. } => min.is_some(),
            Stats::Bool { min, max } => !*min || *max,
            Stats::Decimal { min, .. } => min.is_some(),
        }
    }

    fn min_value(&self) -> Vec<u8> {
        if !self.has_stats() {
            return Vec::new();
        }
        match self {
            Stats::Numeric { min, .. } | Stats::Float { min, .. } => min.as_ref().map(Target::raw).unwrap_or_default(),
            Stats::String(stats) => stats.min.clone(),
            Stats::Uuid { min, .. } => min.clone().unwrap_or_default(),
            Stats::Bool { min, .. } => vec![u8::from(*min)],
            Stats::Decimal { min, .. } => min.map(|v| v.to_be_bytes().to_vec()).unwrap_or_default(),
            Stats::Empty => Vec::new(),
        }
    }

    fn max_value(&self) -> Vec<u8> {
        if !self.has_stats() {
            return Vec::new();
        }
        match self {
            Stats::Numeric { max, .. } | Stats::Float { max, .. } => max.as_ref().map(Target::raw).unwrap_or_default(),
            Stats::String(stats) => stats.max.clone(),
            Stats::Uuid { max, .. } => max.clone().unwrap_or_default(),
            Stats::Bool { max, .. } => vec![u8::from(*max)],
            Stats::Decimal { max, .. } => max.map(|v| v.to_be_bytes().to_vec()).unwrap_or_default(),
            Stats::Empty => Vec::new(),
        }
    }

    /// `GetMin`: the deprecated statistic, which unsigned integers leave out.
    fn deprecated_min_max(&self) -> (Vec<u8>, Vec<u8>) {
        match self {
            Stats::Numeric { signed: false, .. } | Stats::Empty => (Vec::new(), Vec::new()),
            _ => (self.min_value(), self.max_value()),
        }
    }

    fn exact(&self) -> (bool, bool) {
        match self {
            Stats::String(stats) if stats.inexact => (false, false),
            Stats::String(stats) => (!stats.min_truncated, !stats.max_truncated),
            _ => (true, true),
        }
    }

    /// `Merge`: the statistics of a page into those of its column chunk.
    fn merge(&mut self, other: &Stats) {
        match (self, other) {
            (
                Stats::Float {
                    min, max, nan_count, ..
                },
                Stats::Float {
                    min: omin,
                    max: omax,
                    nan_count: other_nans,
                },
            ) => {
                *nan_count += other_nans;
                if let Some(o) = omin
                    && min.as_ref().is_none_or(|m| o.less(m))
                {
                    *min = Some(o.clone());
                }
                if let Some(o) = omax
                    && max.as_ref().is_none_or(|m| m.less(o))
                {
                    *max = Some(o.clone());
                }
            }
            (
                Stats::Numeric { min, max, .. },
                Stats::Numeric {
                    min: omin, max: omax, ..
                },
            ) => {
                if let Some(o) = omin
                    && min.as_ref().is_none_or(|m| o.less(m))
                {
                    *min = Some(o.clone());
                }
                if let Some(o) = omax
                    && max.as_ref().is_none_or(|m| m.less(o))
                {
                    *max = Some(o.clone());
                }
            }
            (Stats::String(a), Stats::String(b)) => {
                if b.failed_truncate {
                    a.failed_truncate = true;
                    a.has_stats = false;
                    a.min.clear();
                    a.max.clear();
                    return;
                }
                if a.failed_truncate || !b.has_stats {
                    return;
                }
                if !a.has_stats || b.min < a.min {
                    a.min.clone_from(&b.min);
                    a.min_truncated = b.min_truncated;
                }
                if !a.has_stats || b.max > a.max {
                    a.max.clone_from(&b.max);
                    a.max_truncated = b.max_truncated;
                }
                a.has_stats = true;
            }
            (Stats::Uuid { min, max }, Stats::Uuid { min: omin, max: omax }) => {
                if let Some(o) = omin
                    && min.as_ref().is_none_or(|m| o < m)
                {
                    *min = Some(o.clone());
                }
                if let Some(o) = omax
                    && max.as_ref().is_none_or(|m| o > m)
                {
                    *max = Some(o.clone());
                }
            }
            (Stats::Bool { min, max }, Stats::Bool { min: omin, max: omax }) => {
                if *omin && !*omax {
                    return;
                }
                *min = *min && *omin;
                *max = *max || *omax;
            }
            (Stats::Decimal { min, max }, Stats::Decimal { min: omin, max: omax }) => {
                for value in [omin, omax].into_iter().flatten() {
                    if min.is_none_or(|m| *value < m) {
                        *min = Some(*value);
                    }
                    if max.is_none_or(|m| *value > m) {
                        *max = Some(*value);
                    }
                }
            }
            _ => {}
        }
    }

    /// `CompareMin` and `CompareMax`: the order of the bounds of two pages.
    fn compare(&self, other: &Stats, max: bool) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        let order = |a: &Target, b: &Target| {
            if a.less(b) {
                Less
            } else if b.less(a) {
                Greater
            } else {
                Equal
            }
        };
        match (self, other) {
            (
                Stats::Numeric { min, max: mx, .. },
                Stats::Numeric {
                    min: omin, max: omx, ..
                },
            )
            | (
                Stats::Float { min, max: mx, .. },
                Stats::Float {
                    min: omin, max: omx, ..
                },
            ) => {
                let (a, b) = if max { (mx, omx) } else { (min, omin) };
                match (a, b) {
                    (Some(a), Some(b)) => order(a, b),
                    _ => Equal,
                }
            }
            (Stats::String(a), Stats::String(b)) => {
                if max {
                    a.max.cmp(&b.max)
                } else {
                    a.min.cmp(&b.min)
                }
            }
            (Stats::Uuid { min, max: mx }, Stats::Uuid { min: omin, max: omx }) => {
                if max {
                    mx.cmp(omx)
                } else {
                    min.cmp(omin)
                }
            }
            (Stats::Bool { min, max: mx }, Stats::Bool { min: omin, max: omx }) => {
                if max {
                    mx.cmp(omx)
                } else {
                    min.cmp(omin)
                }
            }
            (Stats::Decimal { min, max: mx }, Stats::Decimal { min: omin, max: omx }) => {
                if max {
                    mx.cmp(omx)
                } else {
                    min.cmp(omin)
                }
            }
            _ => Equal,
        }
    }

    fn nan_count(&self) -> u64 {
        match self {
            Stats::Float { nan_count, .. } => *nan_count,
            _ => 0,
        }
    }
}

/// The statistics of a column chunk in the footer (`duckdb_parquet::Statistics`).
#[derive(Debug, Clone, Default)]
struct FooterStats {
    max: Option<Vec<u8>>,
    min: Option<Vec<u8>>,
    null_count: Option<i64>,
    distinct_count: Option<i64>,
    max_value: Option<Vec<u8>>,
    min_value: Option<Vec<u8>>,
    is_max_value_exact: Option<bool>,
    is_min_value_exact: Option<bool>,
    nan_count: Option<i64>,
}

impl FooterStats {
    fn write(&self, t: &mut Thrift) {
        if let Some(v) = &self.max {
            t.binary(1, v);
        }
        if let Some(v) = &self.min {
            t.binary(2, v);
        }
        if let Some(v) = self.null_count {
            t.i64(3, v);
        }
        if let Some(v) = self.distinct_count {
            t.i64(4, v);
        }
        if let Some(v) = &self.max_value {
            t.binary(5, v);
        }
        if let Some(v) = &self.min_value {
            t.binary(6, v);
        }
        if let Some(v) = self.is_max_value_exact {
            t.bool(7, v);
        }
        if let Some(v) = self.is_min_value_exact {
            t.bool(8, v);
        }
        if let Some(v) = self.nan_count {
            t.i64(9, v);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The schema: a tree of column writers (`ColumnWriter::CreateWriterRecursive`)
// ---------------------------------------------------------------------------------------------------------------

/// `ParquetColumnSchema`, of what the writer needs.
#[derive(Debug, Clone)]
struct ColumnSchema {
    name: String,
    ty: LogicalType,
    repetition: i32,
    max_define: u16,
    max_repeat: u16,
    field_id: Option<i32>,
}

#[derive(Debug, Clone)]
enum LeafKind {
    /// `StandardColumnWriter`
    Standard {
        convert: Convert,
        stats: StatsKind,
    },
    Boolean,
    /// `EnumColumnWriter`, with the values of the ENUM
    Enum(Vec<String>),
    /// `FixedDecimalColumnWriter`: DECIMALs of 128 bits
    FixedDecimal,
}

#[derive(Debug, Clone)]
struct Leaf {
    schema: ColumnSchema,
    path: Vec<String>,
    kind: LeafKind,
    can_have_nulls: bool,
    /// `bloom_filter_fallback`: a top-level column that gets a bloom filter even without a dictionary
    bloom_fallback: bool,
}

impl Leaf {
    fn physical(&self) -> i32 {
        match &self.kind {
            LeafKind::Standard { convert, .. } => convert.physical(),
            LeafKind::Boolean => physical::BOOLEAN,
            LeafKind::Enum(_) => physical::BYTE_ARRAY,
            LeafKind::FixedDecimal => physical::FIXED_LEN_BYTE_ARRAY,
        }
    }

    /// `BloomFilterEntriesPerValue`: intervals hash twice, as written and normalized.
    fn bloom_entries_per_value(&self) -> u64 {
        match self.kind {
            LeafKind::Standard {
                convert: Convert::Interval,
                ..
            } => 2,
            _ => 1,
        }
    }

    /// `SortednessKey::TRACKED`: whether the values have an order that tells a sorted column.
    fn tracks_sortedness(&self) -> bool {
        matches!(
            self.kind,
            LeafKind::Standard {
                convert: Convert::I32
                    | Convert::U32
                    | Convert::I64
                    | Convert::U64
                    | Convert::TimestampSec
                    | Convert::Int96 { .. }
                    | Convert::Double
                    | Convert::F32
                    | Convert::F64
                    | Convert::Bytes
                    | Convert::Text
                    | Convert::Uuid,
                ..
            }
        )
    }

    /// `TypeIsConstantSize` of the vector's physical type.
    fn constant_size(&self) -> bool {
        !matches!(
            &self.kind,
            LeafKind::Standard {
                convert: Convert::Bytes | Convert::Text,
                ..
            }
        )
    }
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(Leaf),
    Struct {
        schema: ColumnSchema,
        children: Vec<Node>,
        can_have_nulls: bool,
    },
    /// LIST, and MAP (a list of its `key_value` struct)
    List {
        schema: ColumnSchema,
        child: Box<Node>,
        map: bool,
        can_have_nulls: bool,
    },
    Array {
        schema: ColumnSchema,
        child: Box<Node>,
        size: usize,
    },
}

/// `TryGetParquetType`: whether Parquet has the type; the others are written as VARCHAR.
fn has_parquet_type(ty: &LogicalType) -> bool {
    use LogicalTypeId as T;
    matches!(
        ty.id(),
        T::Boolean
            | T::TinyInt
            | T::SmallInt
            | T::Integer
            | T::Date
            | T::BigInt
            | T::Float
            | T::Double
            | T::HugeInt
            | T::UHugeInt
            | T::Enum
            | T::Blob
            | T::Varchar
            | T::Time
            | T::TimeNs
            | T::TimeTz
            | T::Timestamp
            | T::TimestampTz
            | T::TimestampMs
            | T::TimestampNs
            | T::TimestampSec
            | T::UTinyInt
            | T::USmallInt
            | T::UInteger
            | T::UBigInt
            | T::Interval
            | T::Uuid
            | T::SqlNull
            | T::Decimal
    )
}

#[allow(clippy::too_many_arguments)]
fn create_node(
    mut path: Vec<String>,
    ty: &LogicalType,
    name: &str,
    max_repeat: u16,
    mut max_define: u16,
    can_have_nulls: bool,
    options: &WriterOptions,
    field_ids: Option<&FieldIds>,
) -> Result<Node> {
    use LogicalTypeId as T;
    path.push(name.to_string());
    let found = field_ids.and_then(|ids| ids.get(name));
    let field_id = found.and_then(|(id, _)| *id);
    let child_ids = found.map(|(_, children)| children);
    if !can_have_nulls {
        max_define -= 1;
    }
    let repetition = if can_have_nulls { OPTIONAL } else { REQUIRED };
    let schema = |ty: &LogicalType| ColumnSchema {
        name: name.to_string(),
        ty: ty.clone(),
        repetition,
        max_define,
        max_repeat,
        field_id,
    };
    match ty.id() {
        T::Struct | T::Tuple => {
            // `TupleType::NamedChildren`: TUPLE children are `element1`, `element2`, ...
            let fields: Vec<(String, LogicalType)> = ty
                .struct_children()
                .iter()
                .enumerate()
                .map(|(i, (name, child))| {
                    let name = if name.is_empty() {
                        format!("element{}", i + 1)
                    } else {
                        name.clone()
                    };
                    (name, child.clone())
                })
                .collect();
            if fields.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Empty STRUCT columns are not supported in the Parquet format",
                ));
            }
            let children = fields
                .iter()
                .map(|(child_name, child)| {
                    create_node(
                        path.clone(),
                        child,
                        child_name,
                        max_repeat,
                        max_define + 1,
                        true,
                        options,
                        child_ids,
                    )
                })
                .collect::<Result<_>>()?;
            return Ok(Node::Struct {
                schema: schema(ty),
                children,
                can_have_nulls,
            });
        }
        T::List | T::Array => {
            let child_type = ty.child_type().cloned().unwrap_or(LogicalType::INTEGER);
            let mut child_path = path.clone();
            child_path.push("list".to_string());
            let child = create_node(
                child_path,
                &child_type,
                "element",
                max_repeat + 1,
                max_define + 2,
                true,
                options,
                child_ids,
            )?;
            return Ok(if ty.id() == T::List {
                Node::List {
                    schema: schema(ty),
                    child: Box::new(child),
                    map: false,
                    can_have_nulls,
                }
            } else {
                Node::Array {
                    schema: schema(ty),
                    child: Box::new(child),
                    size: ty.array_size().unwrap_or(0) as usize,
                }
            });
        }
        T::Map => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::INTEGER);
            let fields = entry.struct_children();
            let mut child_path = path.clone();
            child_path.push("key_value".to_string());
            let key = create_node(
                child_path.clone(),
                &fields[0].1,
                "key",
                max_repeat + 1,
                max_define + 2,
                false,
                options,
                child_ids,
            )?;
            let value = create_node(
                child_path,
                &fields[1].1,
                "value",
                max_repeat + 1,
                max_define + 2,
                true,
                options,
                child_ids,
            )?;
            let key_value = Node::Struct {
                schema: ColumnSchema {
                    name: "key_value".to_string(),
                    ty: entry.clone(),
                    repetition: REPEATED,
                    max_define: max_define + 1,
                    max_repeat: max_repeat + 1,
                    field_id: None,
                },
                children: vec![key, value],
                can_have_nulls: false,
            };
            return Ok(Node::List {
                schema: schema(ty),
                child: Box::new(key_value),
                map: true,
                can_have_nulls,
            });
        }
        T::Union | T::Variant | T::Geometry => {
            return Err(crate::unported(format!("writing {ty} to a Parquet file")));
        }
        _ => {}
    }
    let leaf_type = if has_parquet_type(ty) {
        ty.clone()
    } else {
        LogicalType::VARCHAR
    };
    let int96 = options.timestamp_as_int96;
    let signed = StatsKind::Numeric { signed: true };
    let kind = match ty.id() {
        _ if !has_parquet_type(ty) => LeafKind::Standard {
            convert: Convert::Text,
            stats: StatsKind::String { blob: false },
        },
        T::Boolean => LeafKind::Boolean,
        T::TinyInt | T::SmallInt | T::Integer | T::Date | T::SqlNull => LeafKind::Standard {
            convert: Convert::I32,
            stats: signed,
        },
        T::UTinyInt | T::USmallInt => LeafKind::Standard {
            convert: Convert::I32,
            stats: StatsKind::Numeric { signed: false },
        },
        T::UInteger => LeafKind::Standard {
            convert: Convert::U32,
            stats: StatsKind::Numeric { signed: false },
        },
        T::UBigInt => LeafKind::Standard {
            convert: Convert::U64,
            stats: StatsKind::Numeric { signed: false },
        },
        T::BigInt | T::Time | T::TimeNs => LeafKind::Standard {
            convert: Convert::I64,
            stats: signed,
        },
        T::Timestamp | T::TimestampTz | T::TimestampMs | T::TimestampNs | T::TimestampSec if int96 => {
            let (divide, multiply) = match ty.id() {
                T::TimestampMs => (1, 1000),
                T::TimestampNs => (1000, 1),
                T::TimestampSec => (1, 1_000_000),
                _ => (1, 1),
            };
            LeafKind::Standard {
                convert: Convert::Int96 { divide, multiply },
                stats: StatsKind::Absent,
            }
        }
        T::Timestamp | T::TimestampTz | T::TimestampMs | T::TimestampNs => LeafKind::Standard {
            convert: Convert::I64,
            stats: signed,
        },
        T::TimestampSec => LeafKind::Standard {
            convert: Convert::TimestampSec,
            stats: signed,
        },
        T::TimeTz => LeafKind::Standard {
            convert: Convert::TimeTz,
            stats: signed,
        },
        T::HugeInt | T::UHugeInt => LeafKind::Standard {
            convert: Convert::Double,
            stats: StatsKind::Empty,
        },
        T::Float => LeafKind::Standard {
            convert: Convert::F32,
            stats: StatsKind::Float,
        },
        T::Double => LeafKind::Standard {
            convert: Convert::F64,
            stats: StatsKind::Float,
        },
        T::Decimal => match ty.decimal_properties().map(|(width, _)| width).unwrap_or(38) {
            0..=9 => LeafKind::Standard {
                convert: Convert::I32,
                stats: signed,
            },
            10..=18 => LeafKind::Standard {
                convert: Convert::I64,
                stats: signed,
            },
            _ => LeafKind::FixedDecimal,
        },
        T::Blob => LeafKind::Standard {
            convert: Convert::Bytes,
            stats: StatsKind::String { blob: true },
        },
        T::Varchar => LeafKind::Standard {
            convert: Convert::Bytes,
            stats: StatsKind::String { blob: false },
        },
        T::Uuid => LeafKind::Standard {
            convert: Convert::Uuid,
            stats: StatsKind::Uuid,
        },
        T::Interval => LeafKind::Standard {
            convert: Convert::Interval,
            stats: StatsKind::Absent,
        },
        T::Enum => LeafKind::Enum(ty.enum_values().to_vec()),
        _ => return Err(crate::unported(format!("writing {ty} to a Parquet file"))),
    };
    Ok(Node::Leaf(Leaf {
        schema: schema(&leaf_type),
        path,
        kind,
        can_have_nulls,
        bloom_fallback: false,
    }))
}

/// A `SchemaElement` of the footer.
#[derive(Debug, Clone, Default)]
struct SchemaElement {
    ty: Option<i32>,
    type_length: Option<i32>,
    repetition: Option<i32>,
    name: String,
    num_children: Option<i32>,
    converted: Option<i32>,
    scale: Option<i32>,
    precision: Option<i32>,
    field_id: Option<i32>,
    logical: Option<LogicalAnnotation>,
}

/// `duckdb_parquet::LogicalType`, of what the writer sets.
#[derive(Debug, Clone, Copy)]
enum LogicalAnnotation {
    Json,
    /// `TIME` with whether it is in UTC, and its unit (1 millis, 2 micros, 3 nanos)
    Time(bool, i16),
    Timestamp(bool, i16),
    Uuid,
    Decimal {
        scale: i32,
        precision: i32,
    },
    Unknown,
}

impl SchemaElement {
    fn write(&self, t: &mut Thrift) {
        if let Some(v) = self.ty {
            t.i32(1, v);
        }
        if let Some(v) = self.type_length {
            t.i32(2, v);
        }
        if let Some(v) = self.repetition {
            t.i32(3, v);
        }
        t.binary(4, self.name.as_bytes());
        if let Some(v) = self.num_children {
            t.i32(5, v);
        }
        if let Some(v) = self.converted {
            t.i32(6, v);
        }
        if let Some(v) = self.scale {
            t.i32(7, v);
        }
        if let Some(v) = self.precision {
            t.i32(8, v);
        }
        if let Some(v) = self.field_id {
            t.i32(9, v);
        }
        if let Some(logical) = self.logical {
            t.structure(10, |t| match logical {
                LogicalAnnotation::Json => t.empty(12),
                LogicalAnnotation::Time(utc, unit) => t.structure(7, |t| {
                    t.bool(1, utc);
                    t.structure(2, |t| t.empty(unit));
                }),
                LogicalAnnotation::Timestamp(utc, unit) => t.structure(8, |t| {
                    t.bool(1, utc);
                    t.structure(2, |t| t.empty(unit));
                }),
                LogicalAnnotation::Uuid => t.empty(14),
                LogicalAnnotation::Decimal { scale, precision } => t.structure(5, |t| {
                    t.i32(1, scale);
                    t.i32(2, precision);
                }),
                LogicalAnnotation::Unknown => t.empty(11),
            });
        }
    }
}

/// `ParquetWriter::SetSchemaProperties`.
fn leaf_schema_element(leaf: &Leaf, options: &WriterOptions) -> SchemaElement {
    use LogicalTypeId as T;
    let ty = &leaf.schema.ty;
    let mut element = SchemaElement {
        ty: Some(leaf.physical()),
        repetition: Some(leaf.schema.repetition),
        name: leaf.schema.name.clone(),
        field_id: leaf.schema.field_id,
        ..Default::default()
    };
    if ty.alias() == Some("JSON") && ty.id() == T::Varchar {
        element.converted = Some(converted::JSON);
        element.logical = Some(LogicalAnnotation::Json);
        return element;
    }
    let int96 = options.timestamp_as_int96;
    let utc = matches!(ty.id(), T::TimestampTz);
    match ty.id() {
        T::TinyInt => element.converted = Some(converted::INT_8),
        T::SmallInt => element.converted = Some(converted::INT_16),
        T::Integer => element.converted = Some(converted::INT_32),
        T::BigInt => element.converted = Some(converted::INT_64),
        T::UTinyInt => element.converted = Some(converted::UINT_8),
        T::USmallInt => element.converted = Some(converted::UINT_16),
        T::UInteger => element.converted = Some(converted::UINT_32),
        T::UBigInt => element.converted = Some(converted::UINT_64),
        T::Date => element.converted = Some(converted::DATE),
        T::Time | T::TimeTz => {
            element.converted = Some(converted::TIME_MICROS);
            element.logical = Some(LogicalAnnotation::Time(ty.id() == T::TimeTz, 2));
        }
        T::TimeNs => element.logical = Some(LogicalAnnotation::Time(false, 3)),
        T::Timestamp | T::TimestampTz | T::TimestampSec if !int96 => {
            element.converted = Some(converted::TIMESTAMP_MICROS);
            element.logical = Some(LogicalAnnotation::Timestamp(utc, 2));
        }
        T::TimestampNs if !int96 => element.logical = Some(LogicalAnnotation::Timestamp(false, 3)),
        T::TimestampMs if !int96 => {
            element.converted = Some(converted::TIMESTAMP_MILLIS);
            element.logical = Some(LogicalAnnotation::Timestamp(false, 1));
        }
        T::Enum | T::Varchar => element.converted = Some(converted::UTF8),
        T::Interval => {
            element.type_length = Some(12);
            element.converted = Some(converted::INTERVAL);
        }
        T::Uuid => {
            element.type_length = Some(16);
            element.logical = Some(LogicalAnnotation::Uuid);
        }
        T::Decimal => {
            let (width, scale) = ty.decimal_properties().unwrap_or((38, 0));
            element.converted = Some(converted::DECIMAL);
            element.precision = Some(i32::from(width));
            element.scale = Some(i32::from(scale));
            if width > 18 {
                element.type_length = Some(16);
            }
            element.logical = Some(LogicalAnnotation::Decimal {
                scale: i32::from(scale),
                precision: i32::from(width),
            });
        }
        T::SqlNull => element.logical = Some(LogicalAnnotation::Unknown),
        _ => {}
    }
    element
}

/// `FinalizeSchema` of a column and the columns in it; returns the number of leaves.
fn finalize_schema(node: &Node, schemas: &mut Vec<SchemaElement>, options: &WriterOptions) -> usize {
    match node {
        Node::Leaf(leaf) => {
            schemas.push(leaf_schema_element(leaf, options));
            1
        }
        Node::Struct { schema, children, .. } => {
            schemas.push(SchemaElement {
                repetition: Some(schema.repetition),
                name: schema.name.clone(),
                num_children: Some(children.len() as i32),
                field_id: schema.field_id,
                ..Default::default()
            });
            children
                .iter()
                .map(|child| finalize_schema(child, schemas, options))
                .sum()
        }
        Node::List { schema, child, map, .. } => {
            schemas.push(SchemaElement {
                repetition: Some(schema.repetition),
                name: schema.name.clone(),
                num_children: Some(1),
                converted: Some(if *map { converted::MAP } else { converted::LIST }),
                field_id: schema.field_id,
                ..Default::default()
            });
            if !*map {
                schemas.push(SchemaElement {
                    repetition: Some(REPEATED),
                    name: "list".to_string(),
                    num_children: Some(1),
                    ..Default::default()
                });
            }
            finalize_schema(child, schemas, options)
        }
        Node::Array { schema, child, .. } => {
            schemas.push(SchemaElement {
                repetition: Some(schema.repetition),
                name: schema.name.clone(),
                num_children: Some(1),
                converted: Some(converted::LIST),
                field_id: schema.field_id,
                ..Default::default()
            });
            schemas.push(SchemaElement {
                repetition: Some(REPEATED),
                name: "list".to_string(),
                num_children: Some(1),
                ..Default::default()
            });
            finalize_schema(child, schemas, options)
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The values of a vector, taken apart as the vectors of its nested types are
// ---------------------------------------------------------------------------------------------------------------

/// A vector of a column: its rows, and for nested types the vectors of their children.
enum Input {
    Leaf(Vec<Value>),
    Struct {
        valid: Vec<bool>,
        children: Vec<Input>,
    },
    List {
        valid: Vec<bool>,
        lengths: Vec<usize>,
        child: Box<Input>,
        child_count: usize,
    },
}

fn build_input(node: &Node, values: Vec<Value>) -> Input {
    match node {
        Node::Leaf(_) => Input::Leaf(values),
        Node::Struct { children, .. } => {
            let valid: Vec<bool> = values.iter().map(|v| !v.is_null()).collect();
            let mut columns: Vec<Vec<Value>> = children.iter().map(|_| Vec::with_capacity(values.len())).collect();
            for value in &values {
                match value.data() {
                    ValueData::Struct(parts) => {
                        for (column, part) in columns.iter_mut().zip(parts) {
                            column.push(part.clone());
                        }
                    }
                    _ => {
                        for column in &mut columns {
                            column.push(Value::sql_null());
                        }
                    }
                }
            }
            Input::Struct {
                valid,
                children: children
                    .iter()
                    .zip(columns)
                    .map(|(child, column)| build_input(child, column))
                    .collect(),
            }
        }
        Node::List { child, .. } => {
            let valid: Vec<bool> = values.iter().map(|v| !v.is_null()).collect();
            let mut lengths = Vec::with_capacity(values.len());
            let mut items = Vec::new();
            for value in &values {
                match value.data() {
                    ValueData::List(entries) => {
                        lengths.push(entries.len());
                        items.extend(entries.iter().cloned());
                    }
                    _ => lengths.push(0),
                }
            }
            let child_count = items.len();
            Input::List {
                valid,
                lengths,
                child: Box::new(build_input(child, items)),
                child_count,
            }
        }
        Node::Array { child, size, .. } => {
            let valid: Vec<bool> = values.iter().map(|v| !v.is_null()).collect();
            let mut items = Vec::with_capacity(values.len() * size);
            for value in &values {
                match value.data() {
                    ValueData::List(entries) => items.extend(entries.iter().cloned()),
                    _ => items.extend(std::iter::repeat_n(Value::sql_null(), *size)),
                }
            }
            let child_count = items.len();
            Input::List {
                valid,
                lengths: vec![*size; values.len()],
                child: Box::new(build_input(child, items)),
                child_count,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Writing a row group
// ---------------------------------------------------------------------------------------------------------------

/// The levels of a column (`ColumnWriterState`).
#[derive(Debug, Default)]
struct Levels {
    definition: Vec<u16>,
    repetition: Vec<u16>,
    is_empty: Vec<bool>,
    null_count: usize,
    parent_null_count: usize,
}

/// `PageInformation`.
#[derive(Debug, Default, Clone)]
struct PageInfo {
    offset: usize,
    row_count: usize,
    empty_count: usize,
    null_count: usize,
    estimated_page_size: usize,
}

/// How a page encodes its values (`StandardWriterPageState` and the others).
enum PageEncoder {
    Plain,
    Dictionary {
        encoder: Box<RleBpEncoder>,
        written: bool,
        bit_width: u32,
    },
    Dbp {
        encoder: DbpEncoder,
        initialized: bool,
    },
    Dlba {
        encoder: DlbaEncoder,
        initialized: bool,
    },
    Bss(BssEncoder),
    Boolean {
        byte: u8,
        position: u8,
    },
}

/// `PageWriteInformation`.
struct PageWrite {
    page_type: i32,
    num_values: usize,
    encoding: i32,
    buffer: Vec<u8>,
    uncompressed_size: usize,
    compressed: Vec<u8>,
    write_count: usize,
    max_write_count: usize,
    encoder: PageEncoder,
    /// with the page index, the statistics of this page alone
    page_stats: Option<Stats>,
    /// the rows of the page, and the values below the maximum definition level
    num_rows: usize,
    null_count: usize,
}

/// The dictionary of a column chunk (`PrimitiveDictionary`): its values in the order they came.
struct Dictionary {
    values: Vec<Target>,
    index: HashMap<Vec<u8>, u32>,
    maximum_size: usize,
    target_size: usize,
    maximum_target_size: usize,
    full: bool,
}

impl Dictionary {
    fn insert(&mut self, value: Target) {
        if self.full {
            return;
        }
        let key = value.key();
        if self.index.contains_key(&key) {
            return;
        }
        let size = value.plain_size();
        if self.values.len() + 1 > self.maximum_size || self.target_size + size > self.maximum_target_size {
            self.full = true;
            return;
        }
        self.target_size += size;
        self.index.insert(key, self.values.len() as u32);
        self.values.push(value);
    }
}

/// `PrimitiveColumnWriterState`.
struct LeafState {
    levels: Levels,
    column: usize,
    page_info: Vec<PageInfo>,
    pages: Vec<PageWrite>,
    current_page: usize,
    encoding: i32,
    dictionary: Option<Dictionary>,
    key_bit_width: u32,
    stats: Option<Stats>,
    bloom: Option<BloomFilter>,
    /// what the bloom filter holds of the budget of fallback filters
    bloom_reserved: usize,
    /// `dictionary_values`: the values analyzed while the dictionary was not full
    dictionary_values: usize,
    sortedness: Sortedness,
    num_values: usize,
    enum_seen: Vec<bool>,
    /// whether every page begins at a row, as the OffsetIndex requires
    pages_start_at_rows: bool,
}

/// `SortednessTracker`: whether the values come in order, ascending or descending.
#[derive(Debug)]
struct Sortedness {
    previous: Option<Target>,
    ascending: bool,
    descending: bool,
}

impl Default for Sortedness {
    fn default() -> Self {
        Self {
            previous: None,
            ascending: true,
            descending: true,
        }
    }
}

impl Sortedness {
    fn is_sorted(&self) -> bool {
        self.ascending || self.descending
    }

    fn update(&mut self, value: Target) {
        if let Some(previous) = &self.previous {
            self.ascending = self.ascending && !greater(previous, &value);
            self.descending = self.descending && !greater(&value, previous);
        }
        self.previous = Some(value);
    }
}

/// `GreaterThan`, where NaN is above every number.
fn greater(a: &Target, b: &Target) -> bool {
    match (a, b) {
        (Target::F32(x), Target::F32(y)) => !y.is_nan() && (x.is_nan() || x > y),
        (Target::F64(x), Target::F64(y)) => !y.is_nan() && (x.is_nan() || x > y),
        _ => b.less(a),
    }
}

/// `EstimateDistinctValues`: the distinct values of `total` rows, from `seen_distinct` of `seen` values drawn
/// from a fixed set.
fn estimate_distinct_values(seen_distinct: usize, seen: usize, total: usize) -> usize {
    if seen_distinct >= seen {
        return total;
    }
    let distinct = seen_distinct as f64;
    let values = seen as f64;
    let expected = |population: f64, draws: f64| -population * (-draws / population).exp_m1();
    let mut low = distinct;
    let mut high = distinct;
    while expected(high, values) < distinct {
        high *= 2.0;
    }
    for _ in 0..64 {
        let mid = (low + high) / 2.0;
        if expected(mid, values) < distinct {
            low = mid;
        } else {
            high = mid;
        }
    }
    let estimate = expected(high, total as f64);
    total.min(estimate.ceil() as usize)
}

/// `ParquetIntervalUtils::TryHashNormalized`, from the bytes of an interval as Parquet stores it.
fn interval_normalized_hash(bytes: &[u8]) -> Option<u64> {
    let part = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("four bytes"));
    let interval = duckrs_common::types::Interval {
        months: part(0) as i32,
        days: part(1) as i32,
        micros: i64::from(part(2)) * 1000,
    };
    let (months, days, micros) = interval.normalize();
    // `Borrow`: what does not fit a field saturates it
    let months = months.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
    let days = days + micros / MICROS_PER_DAY;
    let micros = micros % MICROS_PER_DAY;
    if months < 0 || days < 0 || micros < 0 {
        return None;
    }
    let mut out = (months as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&(days as u32).to_le_bytes());
    out.extend_from_slice(&((micros / 1000) as u32).to_le_bytes());
    Some(xxhash64(&out))
}

/// `InsertBloomFilterValue`.
fn insert_bloom_value(bloom: &mut BloomFilter, value: &Target, interval: bool) {
    let raw = value.raw();
    bloom.insert(xxhash64(&raw));
    if interval && let Some(hash) = interval_normalized_hash(&raw) {
        bloom.insert(hash);
    }
}

enum State {
    Leaf(Box<LeafState>),
    Struct {
        levels: Levels,
        own_null_count: usize,
        children: Vec<State>,
    },
    List {
        levels: Levels,
        parent_index: usize,
        child: Box<State>,
    },
}

/// The metadata of a column chunk (`ColumnChunk` and its `ColumnMetaData`).
struct ChunkMeta {
    physical: i32,
    encodings: Vec<i32>,
    path: Vec<String>,
    num_values: i64,
    total_uncompressed_size: i64,
    total_compressed_size: i64,
    data_page_offset: i64,
    dictionary_page_offset: Option<i64>,
    statistics: Option<FooterStats>,
    encoding_stats: Vec<(i32, i32, i32)>,
    bloom_filter: Option<(i64, i32)>,
    column_index: Option<(i64, i32)>,
    offset_index: Option<(i64, i32)>,
}

struct RowGroupMeta {
    columns: Vec<ChunkMeta>,
    total_byte_size: i64,
    num_rows: i64,
    file_offset: i64,
    total_compressed_size: i64,
}

/// A Parquet file being written (`ParquetWriter`).
pub struct ParquetFileWriter {
    out: std::io::BufWriter<std::fs::File>,
    path: String,
    written: u64,
    options: WriterOptions,
    nodes: Vec<Node>,
    /// the rows not written yet, by column
    buffer: Vec<Vec<Value>>,
    buffered_bytes: usize,
    row_groups: Vec<RowGroupMeta>,
    /// by leaf column and row group, with what they hold of the budget
    blooms: Vec<(usize, usize, BloomFilter, usize)>,
    /// `ParquetBloomFilterMemory`: what fallback filters take, written or not
    bloom_budget_used: usize,
    page_indexes: Vec<PageIndexEntry>,
    num_rows: i64,
}

impl ParquetFileWriter {
    pub fn create(path: &str, names: &[String], types: &[LogicalType], options: WriterOptions) -> Result<Self> {
        let nodes = names
            .iter()
            .zip(types)
            .enumerate()
            .map(|(i, (name, ty))| {
                let can_have_nulls = !options.not_null_columns.get(i).copied().unwrap_or(false);
                create_node(
                    Vec::new(),
                    ty,
                    name,
                    0,
                    1,
                    can_have_nulls,
                    &options,
                    Some(&options.field_ids),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let file = std::fs::File::create(path).map_err(|e| io_error(format!("Cannot open file \"{path}\": {e}")))?;
        let mut writer = Self {
            out: std::io::BufWriter::new(file),
            path: path.to_string(),
            written: 0,
            options,
            buffer: vec![Vec::new(); nodes.len()],
            nodes,
            buffered_bytes: 0,
            row_groups: Vec::new(),
            blooms: Vec::new(),
            bloom_budget_used: 0,
            page_indexes: Vec::new(),
            num_rows: 0,
        };
        if writer.options.bloom_filters {
            for node in &mut writer.nodes {
                if let Node::Leaf(leaf) = node
                    && matches!(leaf.kind, LeafKind::Standard { convert, .. } if !matches!(convert, Convert::Int96 { .. }))
                {
                    leaf.bloom_fallback = true;
                }
            }
            if types.iter().any(contains_interval) {
                writer
                    .options
                    .key_values
                    .push(("duckdb.interval_bloom_filter".to_string(), b"normalized-v1".to_vec()));
            }
        }
        writer.write_bytes(b"PAR1")?;
        Ok(writer)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.out
            .write_all(bytes)
            .map_err(|e| io_error(format!("Could not write \"{}\": {e}", self.path)))?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Takes rows, by column, and writes the row groups they fill.
    pub fn append(&mut self, columns: Vec<Vec<Value>>) -> Result<()> {
        for (buffer, column) in self.buffer.iter_mut().zip(columns) {
            for value in &column {
                self.buffered_bytes += estimated_size(value);
            }
            buffer.extend(column);
        }
        let row_group_size = self.options.row_group_size.max(1);
        while self.buffered_rows() >= row_group_size {
            self.flush_rows(row_group_size)?;
        }
        if let Some(limit) = self.options.row_group_size_bytes
            && self.buffered_bytes >= limit
            && self.buffered_rows() > 0
        {
            self.flush_rows(self.buffered_rows())?;
        }
        Ok(())
    }

    /// What the file holds so far (`ParquetWriteFileSize`).
    pub fn bytes_written(&self) -> usize {
        self.written as usize
    }

    /// The row groups written so far.
    pub fn row_group_count(&self) -> usize {
        self.row_groups.len()
    }

    /// Whether any row came in.
    pub fn has_data(&self) -> bool {
        self.num_rows > 0 || self.buffered_rows() > 0
    }

    /// The rows that fill the row group being buffered.
    pub fn rows_until_row_group(&self) -> usize {
        self.options.row_group_size.max(1) - self.buffered_rows().min(self.options.row_group_size.max(1) - 1)
    }

    fn buffered_rows(&self) -> usize {
        self.buffer.first().map_or(0, Vec::len)
    }

    fn flush_rows(&mut self, rows: usize) -> Result<()> {
        let mut columns = Vec::with_capacity(self.buffer.len());
        for buffer in &mut self.buffer {
            let rest = buffer.split_off(rows);
            columns.push(std::mem::replace(buffer, rest));
        }
        self.buffered_bytes = self.buffer.iter().flatten().map(estimated_size).sum();
        self.write_row_group(columns, rows)
    }

    /// `PrepareRowGroup` and `FlushRowGroup`.
    fn write_row_group(&mut self, columns: Vec<Vec<Value>>, rows: usize) -> Result<()> {
        if rows == 0 {
            return Ok(());
        }
        let nodes = self.nodes.clone();
        let mut leaf_index = 0;
        let mut states = Vec::with_capacity(nodes.len());
        for (node, values) in nodes.iter().zip(columns) {
            // the vectors of the column, as ColumnDataCollection hands them out
            let mut inputs = Vec::new();
            let mut values = values.into_iter();
            loop {
                let chunk: Vec<Value> = values.by_ref().take(STANDARD_VECTOR_SIZE).collect();
                if chunk.is_empty() {
                    break;
                }
                let count = chunk.len();
                inputs.push((build_input(node, chunk), count));
            }
            let mut state = self.initialize_state(node, rows, &mut leaf_index);
            for (input, count) in &inputs {
                analyze(node, &mut state, input, *count, rows, self)?;
            }
            finalize_analyze(node, &mut state, &self.options, &mut self.bloom_budget_used);
            for (input, count) in &inputs {
                prepare(node, &mut state, None, input, *count, true, &self.options)?;
            }
            begin_write(node, &mut state, &self.options);
            for (input, count) in &inputs {
                write(node, &mut state, input, *count, &self.options)?;
            }
            prepare_write(node, &mut state, &self.options)?;
            states.push(state);
        }
        let file_offset = self.written as i64;
        let mut metas = Vec::new();
        let mut total_byte_size = 0;
        let row_group = self.row_groups.len();
        for (node, state) in nodes.iter().zip(states) {
            self.finalize_write(node, state, &mut metas, &mut total_byte_size, row_group)?;
        }
        let total_compressed_size = self.written as i64 - file_offset;
        self.row_groups.push(RowGroupMeta {
            columns: metas,
            total_byte_size,
            num_rows: rows as i64,
            file_offset,
            total_compressed_size,
        });
        self.num_rows += rows as i64;
        Ok(())
    }

    fn initialize_state(&self, node: &Node, rows: usize, leaf_index: &mut usize) -> State {
        match node {
            Node::Leaf(leaf) => {
                let column = *leaf_index;
                *leaf_index += 1;
                let (encoding, dictionary, stats) = match &leaf.kind {
                    LeafKind::Standard { stats, convert } => {
                        let maximum_target_size = if matches!(convert, Convert::Bytes | Convert::Text) {
                            self.options.string_dictionary_page_size_limit
                        } else {
                            usize::MAX
                        };
                        (
                            if self.options.version == 1 {
                                enc::PLAIN_DICTIONARY
                            } else {
                                enc::RLE_DICTIONARY
                            },
                            Some(Dictionary {
                                values: Vec::new(),
                                index: HashMap::new(),
                                maximum_size: self.options.dictionary_size_limit.unwrap_or(rows / 5),
                                target_size: 0,
                                maximum_target_size,
                                full: false,
                            }),
                            Stats::new(*stats),
                        )
                    }
                    LeafKind::Boolean => (enc::PLAIN, None, Some(Stats::Bool { min: true, max: false })),
                    LeafKind::Enum(_) => (
                        enc::RLE_DICTIONARY,
                        None,
                        Some(Stats::String(StringStats::new(false, true))),
                    ),
                    LeafKind::FixedDecimal => (enc::PLAIN, None, Some(Stats::Decimal { min: None, max: None })),
                };
                let enum_count = match &leaf.kind {
                    LeafKind::Enum(values) => values.len(),
                    _ => 0,
                };
                State::Leaf(Box::new(LeafState {
                    levels: Levels::default(),
                    column,
                    page_info: vec![PageInfo::default()],
                    pages: Vec::new(),
                    current_page: 0,
                    encoding,
                    dictionary,
                    key_bit_width: 0,
                    stats,
                    bloom: None,
                    bloom_reserved: 0,
                    dictionary_values: 0,
                    sortedness: Sortedness::default(),
                    pages_start_at_rows: true,
                    num_values: 0,
                    enum_seen: vec![false; enum_count],
                }))
            }
            Node::Struct { children, .. } => State::Struct {
                levels: Levels::default(),
                own_null_count: 0,
                children: children
                    .iter()
                    .map(|child| self.initialize_state(child, rows, leaf_index))
                    .collect(),
            },
            Node::List { child, .. } | Node::Array { child, .. } => State::List {
                levels: Levels::default(),
                parent_index: 0,
                child: Box::new(self.initialize_state(child, rows, leaf_index)),
            },
        }
    }

    /// `FinalizeWrite`: the pages of every leaf to the file, and their metadata.
    fn finalize_write(
        &mut self,
        node: &Node,
        state: State,
        metas: &mut Vec<ChunkMeta>,
        total_byte_size: &mut i64,
        row_group: usize,
    ) -> Result<()> {
        match (node, state) {
            (Node::Leaf(leaf), State::Leaf(state)) => {
                let meta = self.finalize_leaf(leaf, *state, total_byte_size, row_group)?;
                metas.push(meta);
                Ok(())
            }
            (Node::Struct { children, .. }, State::Struct { children: states, .. }) => {
                for (child, state) in children.iter().zip(states) {
                    self.finalize_write(child, state, metas, total_byte_size, row_group)?;
                }
                Ok(())
            }
            (Node::List { child, .. } | Node::Array { child, .. }, State::List { child: state, .. }) => {
                self.finalize_write(child, *state, metas, total_byte_size, row_group)
            }
            _ => Err(io_error("a Parquet column writer without its state")),
        }
    }

    fn finalize_leaf(
        &mut self,
        leaf: &Leaf,
        mut state: LeafState,
        total_byte_size: &mut i64,
        row_group: usize,
    ) -> Result<ChunkMeta> {
        let start = self.written;
        let has_dictionary = has_dictionary(leaf, &state);
        let mut meta = ChunkMeta {
            physical: leaf.physical(),
            encodings: Vec::new(),
            path: leaf.path.clone(),
            num_values: state.num_values as i64,
            total_uncompressed_size: 0,
            total_compressed_size: 0,
            data_page_offset: 0,
            dictionary_page_offset: has_dictionary.then_some(self.written as i64),
            statistics: None,
            encoding_stats: Vec::new(),
            bloom_filter: None,
            column_index: None,
            offset_index: None,
        };
        // `SetParquetStatistics`
        for page in &state.pages {
            let encoding = if page.page_type == DICTIONARY_PAGE {
                enc::PLAIN
            } else {
                page.encoding
            };
            if page.page_type == DATA_PAGE && !meta.encodings.contains(&encoding) {
                meta.encodings.push(encoding);
            }
            match meta
                .encoding_stats
                .iter_mut()
                .find(|(t, e, _)| *t == page.page_type && *e == encoding)
            {
                Some((_, _, count)) => *count += 1,
                None => meta.encoding_stats.push((page.page_type, encoding, 1)),
            }
        }
        if let Some(stats) = &state.stats {
            let mut footer = FooterStats {
                null_count: Some((state.levels.null_count + state.levels.parent_null_count) as i64),
                ..Default::default()
            };
            if matches!(leaf.physical(), physical::FLOAT | physical::DOUBLE) {
                footer.nan_count = Some(stats.nan_count() as i64);
            }
            if stats.nan_count() == 0 {
                let (min, max) = stats.deprecated_min_max();
                if !min.is_empty() {
                    footer.min = Some(min);
                }
                if !max.is_empty() {
                    footer.max = Some(max);
                }
                if stats.has_stats() {
                    let (min_exact, max_exact) = stats.exact();
                    footer.min_value = Some(stats.min_value());
                    footer.max_value = Some(stats.max_value());
                    footer.is_min_value_exact = Some(min_exact);
                    footer.is_max_value_exact = Some(max_exact);
                }
            }
            if has_dictionary {
                footer.distinct_count = Some(dictionary_size(leaf, &state) as i64);
            }
            meta.statistics = Some(footer);
        }
        let mut total_uncompressed = 0usize;
        let mut index = PageIndexEntry {
            column: state.column,
            row_group,
            has_column_index: false,
            null_pages: Vec::new(),
            min_values: Vec::new(),
            max_values: Vec::new(),
            boundary_order: 0,
            null_counts: Vec::new(),
            locations: Vec::new(),
        };
        let mut first_row = 0i64;
        let pages = std::mem::take(&mut state.pages);
        for page in &pages {
            let header = page_header(page);
            let offset = self.written as i64;
            if meta.data_page_offset == 0 && page.page_type == DATA_PAGE {
                meta.data_page_offset = offset;
            }
            if self.options.write_page_index && page.page_type == DATA_PAGE {
                // `AddPageToPageIndex`
                index
                    .locations
                    .push((offset, (header.len() + page.compressed.len()) as i32, first_row));
                first_row += page.num_rows as i64;
                let null_page = page.page_stats.as_ref().is_none_or(|stats| !stats.has_stats());
                index.null_pages.push(null_page);
                let stats = page.page_stats.as_ref();
                index.min_values.push(if null_page {
                    Vec::new()
                } else {
                    stats.map(Stats::min_value).unwrap_or_default()
                });
                index.max_values.push(if null_page {
                    Vec::new()
                } else {
                    stats.map(Stats::max_value).unwrap_or_default()
                });
                index.null_counts.push(page.null_count as i64);
            }
            total_uncompressed += header.len() + page.uncompressed_size;
            self.write_bytes(&header)?;
            self.write_bytes(&page.compressed)?;
        }
        // a chunk of a single page has no page index: its statistics describe that page
        if self.options.write_page_index && state.pages_start_at_rows && index.locations.len() > 1 {
            finalize_page_index(&pages, &mut index);
            self.page_indexes.push(index);
        }
        meta.total_compressed_size = (self.written - start) as i64;
        meta.total_uncompressed_size = total_uncompressed as i64;
        *total_byte_size += meta.total_uncompressed_size;
        if let Some(bloom) = state.bloom.take() {
            let keep = has_dictionary
                || bloom.data.len() * MIN_COMPRESSED_TO_FILTER_RATIO <= meta.total_compressed_size as usize;
            if keep {
                self.blooms.push((state.column, row_group, bloom, state.bloom_reserved));
            } else {
                self.bloom_budget_used -= state.bloom_reserved;
            }
        }
        Ok(meta)
    }

    /// `ParquetBloomFilterMemory::TryReserve`: makes room by writing the bloom filters that are done.
    fn try_reserve(&mut self, bytes: usize) -> bool {
        let limit = self.options.bloom_filter_fallback_max_bytes;
        if self.bloom_budget_used + bytes <= limit {
            self.bloom_budget_used += bytes;
            return true;
        }
        if bytes > limit || self.flush_blooms().is_err() {
            return false;
        }
        if self.bloom_budget_used + bytes <= limit {
            self.bloom_budget_used += bytes;
            return true;
        }
        false
    }

    /// `FlushBloomFilters`, by column and then by row group.
    fn flush_blooms(&mut self) -> Result<()> {
        let mut blooms = std::mem::take(&mut self.blooms);
        blooms.sort_by_key(|(column, row_group, _, _)| (*column, *row_group));
        for (column, row_group, bloom, reserved) in blooms {
            self.bloom_budget_used -= reserved;
            let offset = self.written as i64;
            let mut t = Thrift::new();
            t.begin();
            t.i32(1, bloom.data.len() as i32);
            t.structure(2, |t| t.empty(1));
            t.structure(3, |t| t.empty(1));
            t.structure(4, |t| t.empty(1));
            t.end();
            self.write_bytes(&t.out)?;
            self.write_bytes(&bloom.data)?;
            let length = (t.out.len() + bloom.data.len()) as i32;
            if let Some(meta) = self
                .row_groups
                .get_mut(row_group)
                .and_then(|g| g.columns.get_mut(column))
            {
                meta.bloom_filter = Some((offset, length));
            }
        }
        Ok(())
    }

    /// `ParquetWriter::Finalize`: the rest of the rows, the bloom filters and the footer.
    pub fn finish(mut self) -> Result<u64> {
        let rows = self.buffered_rows();
        self.flush_rows(rows)?;
        self.flush_blooms()?;
        // the ColumnIndex of every column chunk, then the OffsetIndex of every one
        let indexes = std::mem::take(&mut self.page_indexes);
        for entry in indexes.iter().filter(|entry| entry.has_column_index) {
            let bytes = entry.column_index();
            let at = (self.written as i64, bytes.len() as i32);
            self.write_bytes(&bytes)?;
            if let Some(meta) = self.row_groups[entry.row_group].columns.get_mut(entry.column) {
                meta.column_index = Some(at);
            }
        }
        for entry in &indexes {
            let bytes = entry.offset_index();
            let at = (self.written as i64, bytes.len() as i32);
            self.write_bytes(&bytes)?;
            if let Some(meta) = self.row_groups[entry.row_group].columns.get_mut(entry.column) {
                meta.offset_index = Some(at);
            }
        }
        let footer = self.footer();
        self.write_bytes(&footer)?;
        self.write_bytes(&(footer.len() as u32).to_le_bytes())?;
        self.write_bytes(b"PAR1")?;
        self.out
            .flush()
            .map_err(|e| io_error(format!("Could not write \"{}\": {e}", self.path)))?;
        Ok(self.num_rows as u64)
    }

    fn footer(&self) -> Vec<u8> {
        let mut schemas = vec![SchemaElement {
            repetition: Some(REQUIRED),
            name: "duckdb_schema".to_string(),
            num_children: Some(self.nodes.len() as i32),
            ..Default::default()
        }];
        let leaves: usize = self
            .nodes
            .iter()
            .map(|node| finalize_schema(node, &mut schemas, &self.options))
            .sum();
        let codec = self.options.codec.thrift();
        let mut t = Thrift::new();
        t.begin();
        t.i32(1, self.options.version);
        t.list(2, ttype::STRUCT, schemas.len());
        for schema in &schemas {
            t.element_struct(|t| schema.write(t));
        }
        t.i64(3, self.num_rows);
        t.list(4, ttype::STRUCT, self.row_groups.len());
        for row_group in &self.row_groups {
            t.element_struct(|t| {
                t.list(1, ttype::STRUCT, row_group.columns.len());
                for column in &row_group.columns {
                    t.element_struct(|t| {
                        t.i64(2, 0);
                        t.structure(3, |t| write_column_meta(t, column, codec));
                        if let Some((offset, length)) = column.offset_index {
                            t.i64(4, offset);
                            t.i32(5, length);
                        }
                        if let Some((offset, length)) = column.column_index {
                            t.i64(6, offset);
                            t.i32(7, length);
                        }
                    });
                }
                t.i64(2, row_group.total_byte_size);
                t.i64(3, row_group.num_rows);
                t.i64(5, row_group.file_offset);
                t.i64(6, row_group.total_compressed_size);
            });
        }
        if !self.options.key_values.is_empty() {
            t.list(5, ttype::STRUCT, self.options.key_values.len());
            for (key, value) in &self.options.key_values {
                t.element_struct(|t| {
                    t.binary(1, key.as_bytes());
                    t.binary(2, value);
                });
            }
        }
        t.binary(6, CREATED_BY.as_bytes());
        t.list(7, ttype::STRUCT, leaves);
        for _ in 0..leaves {
            t.element_struct(|t| t.empty(1));
        }
        t.end();
        t.out
    }
}

fn write_column_meta(t: &mut Thrift, column: &ChunkMeta, codec: i32) {
    t.i32(1, column.physical);
    t.list(2, ttype::I32, column.encodings.len());
    for encoding in &column.encodings {
        t.element_i32(*encoding);
    }
    t.list(3, ttype::BINARY, column.path.len());
    for part in &column.path {
        t.raw_binary(part.as_bytes());
    }
    t.i32(4, codec);
    t.i64(5, column.num_values);
    t.i64(6, column.total_uncompressed_size);
    t.i64(7, column.total_compressed_size);
    t.i64(9, column.data_page_offset);
    if let Some(offset) = column.dictionary_page_offset {
        t.i64(11, offset);
    }
    if let Some(stats) = &column.statistics {
        t.structure(12, |t| stats.write(t));
    }
    if !column.encoding_stats.is_empty() {
        t.list(13, ttype::STRUCT, column.encoding_stats.len());
        for (page_type, encoding, count) in &column.encoding_stats {
            t.element_struct(|t| {
                t.i32(1, *page_type);
                t.i32(2, *encoding);
                t.i32(3, *count);
            });
        }
    }
    if let Some((offset, length)) = column.bloom_filter {
        t.i64(14, offset);
        t.i32(15, length);
    }
}

fn page_header(page: &PageWrite) -> Vec<u8> {
    let mut t = Thrift::new();
    t.begin();
    t.i32(1, page.page_type);
    t.i32(2, page.uncompressed_size as i32);
    t.i32(3, page.compressed.len() as i32);
    if page.page_type == DICTIONARY_PAGE {
        t.structure(7, |t| {
            t.i32(1, page.num_values as i32);
            t.i32(2, enc::PLAIN);
        });
    } else {
        t.structure(5, |t| {
            t.i32(1, page.num_values as i32);
            t.i32(2, page.encoding);
            t.i32(3, enc::RLE);
            t.i32(4, enc::RLE);
        });
    }
    t.end();
    t.out
}

/// About what a value takes in memory, for `row_group_size_bytes`.
fn estimated_size(value: &Value) -> usize {
    match value.data() {
        ValueData::Bytes(bytes) => 16 + bytes.len(),
        ValueData::List(items) | ValueData::Struct(items) => 16 + items.iter().map(estimated_size).sum::<usize>(),
        ValueData::Int128(_) | ValueData::UInt128(_) | ValueData::Interval(_) => 16,
        _ => 8,
    }
}

fn has_dictionary(leaf: &Leaf, state: &LeafState) -> bool {
    match leaf.kind {
        LeafKind::Enum(_) => true,
        LeafKind::Standard { .. } => matches!(state.encoding, enc::PLAIN_DICTIONARY | enc::RLE_DICTIONARY),
        _ => false,
    }
}

fn dictionary_size(leaf: &Leaf, state: &LeafState) -> usize {
    match leaf.kind {
        LeafKind::Enum(_) => state.enum_seen.iter().filter(|seen| **seen).count(),
        _ => state.dictionary.as_ref().map_or(0, |d| d.values.len()),
    }
}

fn converted_value(leaf: &Leaf, value: &Value) -> Result<Target> {
    match &leaf.kind {
        LeafKind::Standard { convert, .. } => convert.apply(value),
        _ => Err(io_error("not a standard Parquet column")),
    }
}

/// `Analyze`: the dictionaries of the leaves, and the bloom filters of those that outgrow them.
fn analyze(
    node: &Node,
    state: &mut State,
    input: &Input,
    count: usize,
    rows: usize,
    writer: &mut ParquetFileWriter,
) -> Result<()> {
    match (node, state, input) {
        (Node::Leaf(leaf), State::Leaf(state), Input::Leaf(values)) => {
            let LeafKind::Standard { .. } = leaf.kind else {
                return Ok(());
            };
            if state.dictionary.is_none() {
                return Ok(());
            }
            let values = &values[..count.min(values.len())];
            // `TracksSortedness`
            let full = state.dictionary.as_ref().is_some_and(|d| d.full);
            let track = leaf.bloom_fallback
                && leaf.tracks_sortedness()
                && state.sortedness.is_sorted()
                && (!full || state.bloom.is_some());
            if !track {
                state.sortedness.ascending &= !leaf.bloom_fallback || leaf.tracks_sortedness();
                state.sortedness.descending &= !leaf.bloom_fallback || leaf.tracks_sortedness();
            }
            let all_valid = values.iter().all(|v| !v.is_null());
            if all_valid {
                if track {
                    for value in values {
                        if !state.sortedness.is_sorted() {
                            break;
                        }
                        state.sortedness.update(converted_value(leaf, value)?);
                    }
                }
                for value in values {
                    analyze_value(leaf, state, converted_value(leaf, value)?, rows, writer);
                }
            } else {
                for value in values.iter().filter(|v| !v.is_null()) {
                    let target = converted_value(leaf, value)?;
                    if track {
                        state.sortedness.update(target.clone());
                    }
                    analyze_value(leaf, state, target, rows, writer);
                }
            }
            Ok(())
        }
        (
            Node::Struct { children, .. },
            State::Struct { children: states, .. },
            Input::Struct { children: inputs, .. },
        ) => {
            for ((child, state), input) in children.iter().zip(states).zip(inputs) {
                analyze(child, state, input, count, rows, writer)?;
            }
            Ok(())
        }
        (
            Node::List { child, .. } | Node::Array { child, .. },
            State::List { child: state, .. },
            Input::List {
                child: input,
                child_count,
                ..
            },
        ) => analyze(child, state, input, *child_count, rows, writer),
        _ => Ok(()),
    }
}

/// `AnalyzeValue`: `CountAnalyzedValue` and `InsertAnalyzedValue`.
fn analyze_value(leaf: &Leaf, state: &mut LeafState, value: Target, rows: usize, writer: &mut ParquetFileWriter) {
    let interval = leaf.bloom_entries_per_value() == 2;
    let dictionary = state.dictionary.as_mut().expect("a dictionary");
    if !dictionary.full {
        state.dictionary_values += 1;
        let before = dictionary.full;
        dictionary.insert(value.clone());
        if !dictionary.full {
            return;
        }
        if !before {
            start_bloom_fallback(leaf, state, rows, writer);
        }
    }
    if let Some(bloom) = state.bloom.as_mut() {
        insert_bloom_value(bloom, &value, interval);
    }
}

/// `StartBloomFilterFallback`: a dictionary that is full hands its values to a bloom filter for all the rows.
fn start_bloom_fallback(leaf: &Leaf, state: &mut LeafState, rows: usize, writer: &mut ParquetFileWriter) {
    const MIN_PREFIX_VALUES: usize = 1024;
    const MIN_COLUMN_TO_FILTER_RATIO: usize = 4;
    if !leaf.bloom_fallback {
        return;
    }
    let fpp = writer.options.bloom_filter_fpp;
    let entries = leaf.bloom_entries_per_value();
    let dictionary = state.dictionary.as_ref().expect("a dictionary");
    // `SkipBloomFilterFallback`
    if state.dictionary_values >= MIN_PREFIX_VALUES {
        if state.sortedness.is_sorted() {
            return;
        }
        let distinct = estimate_distinct_values(dictionary.values.len() + 1, state.dictionary_values, rows);
        let Some(filter_size) = BloomFilter::size(distinct as u64 * entries, fpp) else {
            return;
        };
        let value_size = dictionary.target_size / dictionary.values.len().max(1);
        if filter_size * MIN_COLUMN_TO_FILTER_RATIO > value_size * rows {
            return;
        }
    }
    // `ParquetBloomFilter::TryCreate`
    if rows == 0 {
        return;
    }
    let Some(size) = BloomFilter::size(rows as u64 * entries, fpp) else {
        return;
    };
    let reservation = size + size / 2;
    if !writer.try_reserve(reservation) {
        return;
    }
    let mut bloom = BloomFilter { data: vec![0; size] };
    let interval = entries == 2;
    let mut seedable = true;
    let dictionary = state.dictionary.as_mut().expect("a dictionary");
    for value in &dictionary.values {
        // `CanSeedBloomFilter`: a NaN can't be looked up, so the filter would pass over it
        seedable &= !matches!(value, Target::F32(v) if v.is_nan()) && !matches!(value, Target::F64(v) if v.is_nan());
        insert_bloom_value(&mut bloom, value, interval);
    }
    dictionary.values.clear();
    dictionary.index.clear();
    if seedable {
        state.bloom = Some(bloom);
        state.bloom_reserved = reservation;
    } else {
        writer.bloom_budget_used -= reservation;
    }
}

fn contains_interval(ty: &LogicalType) -> bool {
    match ty.id() {
        LogicalTypeId::Interval => true,
        LogicalTypeId::Struct => ty.struct_children().iter().any(|(_, child)| contains_interval(child)),
        LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Map => {
            ty.child_type().is_some_and(contains_interval)
        }
        _ => false,
    }
}

/// `FinalizeAnalyze`: dictionary encoding, or else what is better than PLAIN.
fn finalize_analyze(node: &Node, state: &mut State, options: &WriterOptions, budget: &mut usize) {
    match (node, state) {
        (Node::Leaf(leaf), State::Leaf(state)) => {
            let LeafKind::Standard { convert, .. } = leaf.kind else {
                return;
            };
            let dictionary = state.dictionary.as_ref().expect("standard columns have a dictionary");
            if dictionary.values.is_empty() || dictionary.full {
                state.dictionary = None;
                state.encoding = if options.version == 1 {
                    enc::PLAIN
                } else {
                    match convert.physical() {
                        physical::INT32 | physical::INT64 => enc::DELTA_BINARY_PACKED,
                        physical::BYTE_ARRAY => enc::DELTA_LENGTH_BYTE_ARRAY,
                        physical::FLOAT | physical::DOUBLE => enc::BYTE_STREAM_SPLIT,
                        _ => enc::PLAIN,
                    }
                };
            } else {
                state.key_bit_width = bit_width_of_count(dictionary.values.len() as u64);
            }
            if state.bloom.is_some() && state.sortedness.is_sorted() {
                state.bloom = None;
                *budget -= std::mem::take(&mut state.bloom_reserved);
            }
            if let Some(bloom) = state.bloom.as_mut() {
                bloom.fold_to_target(options.bloom_filter_fpp);
                *budget -= state.bloom_reserved - bloom.data.len();
                state.bloom_reserved = bloom.data.len();
            }
        }
        (Node::Struct { children, .. }, State::Struct { children: states, .. }) => {
            for (child, state) in children.iter().zip(states) {
                finalize_analyze(child, state, options, budget);
            }
        }
        (Node::List { child, .. } | Node::Array { child, .. }, State::List { child: state, .. }) => {
            finalize_analyze(child, state, options, budget)
        }
        _ => {}
    }
}

/// `HandleRepeatLevels`.
fn handle_repeat_levels(levels: &mut Levels, parent: Option<&Levels>) {
    let Some(parent) = parent else {
        return;
    };
    if levels.repetition.len() < parent.repetition.len() {
        let from = levels.repetition.len();
        levels.repetition.extend_from_slice(&parent.repetition[from..]);
    }
}

/// `HandleDefineLevels`.
fn handle_define_levels(
    levels: &mut Levels,
    parent: Option<&Levels>,
    valid: &dyn Fn(usize) -> bool,
    count: usize,
    define_value: u16,
    null_value: u16,
    can_have_nulls: bool,
) -> Result<()> {
    let map_key_error = || io_error("Parquet writer: map key column is not allowed to contain NULL values");
    if let Some(parent) = parent {
        let mut vector_index = 0;
        while levels.definition.len() < parent.definition.len() {
            let current = levels.definition.len();
            if parent.definition[current] != DEFINE_VALID {
                levels.definition.push(parent.definition[current]);
                levels.parent_null_count += 1;
            } else if valid(vector_index) {
                levels.definition.push(define_value);
            } else {
                if !can_have_nulls {
                    return Err(map_key_error());
                }
                levels.null_count += 1;
                levels.definition.push(null_value);
            }
            if parent.is_empty.is_empty() || !parent.is_empty[current] {
                vector_index += 1;
            }
        }
        return Ok(());
    }
    for i in 0..count {
        if valid(i) {
            levels.definition.push(define_value);
        } else {
            levels.definition.push(null_value);
            levels.null_count += 1;
        }
    }
    if !can_have_nulls && levels.null_count != 0 {
        return Err(map_key_error());
    }
    Ok(())
}

/// `GetRowSize`.
fn row_size(leaf: &Leaf, state: &LeafState, value: &Value) -> usize {
    match &leaf.kind {
        LeafKind::Standard { convert, .. } => {
            if matches!(state.encoding, enc::PLAIN_DICTIONARY | enc::RLE_DICTIONARY) {
                return state.key_bit_width.div_ceil(8) as usize;
            }
            match convert {
                Convert::Bytes | Convert::Text => {
                    4 + match value.data() {
                        ValueData::Bytes(bytes) => bytes.len(),
                        _ => value.try_to_string().map(|s| s.len()).unwrap_or(0),
                    }
                }
                Convert::I32 | Convert::U32 | Convert::F32 => 4,
                Convert::Int96 { .. } | Convert::Interval => 12,
                Convert::Uuid => 16,
                _ => 8,
            }
        }
        LeafKind::Boolean => 1,
        LeafKind::Enum(values) => bit_width_of_count(values.len() as u64).div_ceil(8) as usize,
        LeafKind::FixedDecimal => 16,
    }
}

/// `Prepare`: the levels, and where pages begin.
fn prepare(
    node: &Node,
    state: &mut State,
    parent: Option<&Levels>,
    input: &Input,
    count: usize,
    can_span: bool,
    options: &WriterOptions,
) -> Result<()> {
    match (node, state, input) {
        (Node::Leaf(leaf), State::Leaf(state), Input::Leaf(values)) => {
            let vcount = match parent {
                Some(parent) => parent.definition.len() - state.levels.definition.len(),
                None => count,
            };
            let parent_index = state.levels.definition.len();
            handle_repeat_levels(&mut state.levels, parent);
            let max_define = leaf.schema.max_define;
            handle_define_levels(
                &mut state.levels,
                parent,
                &|i| values.get(i).is_some_and(|v| !v.is_null()),
                count,
                max_define,
                max_define.saturating_sub(1),
                leaf.can_have_nulls,
            )?;
            state.num_values += vcount;
            let limit = options.data_page_size_limit;
            let check_parent_empty = parent.is_some_and(|p| !p.is_empty.is_empty());
            let all_valid = values.iter().take(count).all(|v| !v.is_null());
            let fixed = leaf.constant_size();
            if !check_parent_empty && all_valid && fixed {
                let size = values.first().map_or(0, |v| row_size(leaf, state, v));
                let page = state.page_info.last_mut().expect("a page");
                if page.estimated_page_size + size * vcount < limit {
                    page.row_count += vcount;
                    page.estimated_page_size += size * vcount;
                    return Ok(());
                }
            }
            let mut vector_index = 0;
            for i in 0..vcount {
                if i == 0 && !can_span && options.write_page_index {
                    // the values of a list start on a new page when this one is full, so that every page
                    // begins at a row as the OffsetIndex requires
                    let page = state.page_info.last().expect("a page");
                    if page.row_count > 0 && page.estimated_page_size >= limit {
                        let offset = page.offset + page.row_count;
                        state.page_info.push(PageInfo {
                            offset,
                            ..Default::default()
                        });
                    }
                }
                let page = state.page_info.last_mut().expect("a page");
                page.row_count += 1;
                if check_parent_empty && parent.is_some_and(|p| p.is_empty[parent_index + i]) {
                    page.empty_count += 1;
                    continue;
                }
                let value = values.get(vector_index);
                if value.is_some_and(|v| !v.is_null()) {
                    let size = row_size(leaf, state, value.expect("checked"));
                    let page = state.page_info.last_mut().expect("a page");
                    page.estimated_page_size += size;
                    if page.estimated_page_size >= limit && (can_span || (i == 0 && !options.write_page_index)) {
                        let offset = page.offset + page.row_count;
                        state.page_info.push(PageInfo {
                            offset,
                            ..Default::default()
                        });
                    }
                } else {
                    page.null_count += 1;
                }
                vector_index += 1;
            }
            Ok(())
        }
        (
            Node::Struct {
                schema,
                children,
                can_have_nulls,
            },
            State::Struct {
                levels,
                own_null_count,
                children: states,
            },
            Input::Struct {
                valid,
                children: inputs,
            },
        ) => {
            if let Some(parent) = parent
                && levels.is_empty.len() < parent.is_empty.len()
            {
                let from = levels.is_empty.len();
                levels.is_empty.extend_from_slice(&parent.is_empty[from..]);
            }
            handle_repeat_levels(levels, parent);
            handle_define_levels(
                levels,
                parent,
                &|i| valid.get(i).copied().unwrap_or(false),
                count,
                DEFINE_VALID,
                schema.max_define.saturating_sub(1),
                *can_have_nulls || schema.repetition != REPEATED,
            )?;
            *own_null_count = levels.null_count;
            for ((child, state), input) in children.iter().zip(states).zip(inputs) {
                prepare(child, state, Some(levels), input, count, can_span, options)?;
            }
            Ok(())
        }
        (
            Node::List {
                schema,
                child,
                can_have_nulls,
                ..
            },
            State::List {
                levels,
                parent_index,
                child: state,
            },
            Input::List {
                valid,
                lengths,
                child: input,
                child_count,
            },
        ) => {
            let vcount = match parent {
                Some(parent) => parent.definition.len() - *parent_index,
                None => count,
            };
            let max_repeat = schema.max_repeat;
            let max_define = schema.max_define;
            let mut vector_index = 0;
            for i in 0..vcount {
                let at = *parent_index + i;
                if let Some(parent) = parent
                    && !parent.is_empty.is_empty()
                    && parent.is_empty[at]
                {
                    levels.definition.push(parent.definition[at]);
                    levels.repetition.push(parent.repetition[at]);
                    levels.is_empty.push(true);
                    continue;
                }
                let first_repeat = match parent {
                    Some(parent) if !parent.repetition.is_empty() => parent.repetition[at],
                    _ => max_repeat,
                };
                if let Some(parent) = parent
                    && parent.definition[at] != DEFINE_VALID
                {
                    levels.definition.push(parent.definition[at]);
                    levels.repetition.push(first_repeat);
                    levels.is_empty.push(true);
                } else if valid.get(vector_index).copied().unwrap_or(false) {
                    let length = lengths[vector_index];
                    if length == 0 {
                        levels.definition.push(max_define);
                        levels.is_empty.push(true);
                    } else {
                        levels.definition.push(DEFINE_VALID);
                        levels.is_empty.push(false);
                    }
                    levels.repetition.push(first_repeat);
                    for _ in 1..length {
                        levels.repetition.push(max_repeat + 1);
                        levels.definition.push(DEFINE_VALID);
                        levels.is_empty.push(false);
                    }
                } else {
                    if !*can_have_nulls {
                        return Err(io_error(
                            "Parquet writer: map key column is not allowed to contain NULL values",
                        ));
                    }
                    levels.definition.push(max_define.saturating_sub(1));
                    levels.repetition.push(first_repeat);
                    levels.is_empty.push(true);
                }
                vector_index += 1;
            }
            *parent_index += vcount;
            prepare(child, state, Some(levels), input, *child_count, false, options)
        }
        (
            Node::Array { schema, child, size },
            State::List {
                levels,
                parent_index,
                child: state,
            },
            Input::List {
                valid,
                child: input,
                child_count,
                ..
            },
        ) => {
            let vcount = match parent {
                Some(parent) => parent.definition.len() - *parent_index,
                None => count,
            };
            let max_repeat = schema.max_repeat;
            let push = |levels: &mut Levels, first_repeat: u16, define: u16, empty: bool| {
                levels.definition.push(define);
                levels.repetition.push(first_repeat);
                levels.is_empty.push(empty);
                if empty {
                    return;
                }
                for _ in 1..*size {
                    levels.repetition.push(max_repeat + 1);
                    levels.definition.push(define);
                    levels.is_empty.push(false);
                }
            };
            let mut vector_index = 0;
            for i in 0..vcount {
                let at = *parent_index + i;
                if let Some(parent) = parent
                    && !parent.is_empty.is_empty()
                    && parent.is_empty[at]
                {
                    push(levels, parent.repetition[at], parent.definition[at], true);
                    continue;
                }
                let first_repeat = match parent {
                    Some(parent) if !parent.repetition.is_empty() => parent.repetition[at],
                    _ => max_repeat,
                };
                if let Some(parent) = parent
                    && parent.definition[at] != DEFINE_VALID
                {
                    push(levels, first_repeat, parent.definition[at], false);
                } else if valid.get(vector_index).copied().unwrap_or(false) {
                    push(levels, first_repeat, DEFINE_VALID, false);
                } else {
                    push(levels, first_repeat, schema.max_define.saturating_sub(1), false);
                }
                vector_index += 1;
            }
            *parent_index += vcount;
            prepare(child, state, Some(levels), input, *child_count, false, options)
        }
        _ => Err(io_error("a Parquet column writer without its state")),
    }
}

fn page_encoder(leaf: &Leaf, state: &LeafState, info: &PageInfo) -> PageEncoder {
    let values = info.row_count - (info.empty_count + info.null_count);
    match &leaf.kind {
        LeafKind::Boolean => PageEncoder::Boolean { byte: 0, position: 0 },
        LeafKind::Enum(entries) => PageEncoder::Dictionary {
            encoder: Box::new(RleBpEncoder::new(bit_width_of_count(entries.len() as u64))),
            written: false,
            bit_width: bit_width_of_count(entries.len() as u64),
        },
        LeafKind::FixedDecimal => PageEncoder::Plain,
        LeafKind::Standard { convert, .. } => match state.encoding {
            enc::PLAIN_DICTIONARY | enc::RLE_DICTIONARY => {
                let size = state.dictionary.as_ref().map_or(0, |d| d.values.len());
                let bit_width = bit_width_of_count(size as u64);
                PageEncoder::Dictionary {
                    encoder: Box::new(RleBpEncoder::new(bit_width)),
                    written: false,
                    bit_width,
                }
            }
            enc::DELTA_BINARY_PACKED => PageEncoder::Dbp {
                encoder: DbpEncoder::new(values as u64),
                initialized: false,
            },
            enc::DELTA_LENGTH_BYTE_ARRAY => PageEncoder::Dlba {
                encoder: DlbaEncoder::new(values as u64),
                initialized: false,
            },
            enc::BYTE_STREAM_SPLIT => PageEncoder::Bss(BssEncoder::new(
                values,
                if convert.physical() == physical::FLOAT { 4 } else { 8 },
            )),
            _ => PageEncoder::Plain,
        },
    }
}

/// `BeginWrite`.
fn begin_write(node: &Node, state: &mut State, options: &WriterOptions) {
    match (node, state) {
        (Node::Leaf(leaf), State::Leaf(state)) => {
            if state.page_info.last().is_some_and(|p| p.row_count == 0) {
                state.page_info.pop();
            }
            let mut pages = Vec::with_capacity(state.page_info.len());
            for info in &state.page_info {
                let (mut num_rows, mut null_count, mut page_stats) = (info.row_count, 0, None);
                if options.write_page_index {
                    page_stats = fresh_stats(leaf);
                    let range = info.offset..info.offset + info.row_count;
                    if leaf.schema.max_repeat > 0 {
                        num_rows = state.levels.repetition[range.clone()]
                            .iter()
                            .filter(|r| **r == 0)
                            .count();
                        if state.levels.repetition.get(info.offset).is_some_and(|r| *r != 0) {
                            state.pages_start_at_rows = false;
                        }
                    }
                    if leaf.schema.max_define > 0 {
                        null_count = state.levels.definition[range]
                            .iter()
                            .filter(|d| **d != leaf.schema.max_define)
                            .count();
                    }
                }
                pages.push(PageWrite {
                    page_type: DATA_PAGE,
                    num_values: info.row_count,
                    encoding: state.encoding,
                    buffer: Vec::new(),
                    uncompressed_size: 0,
                    compressed: Vec::new(),
                    write_count: info.empty_count,
                    max_write_count: info.row_count,
                    encoder: page_encoder(leaf, state, info),
                    page_stats,
                    num_rows,
                    null_count,
                });
            }
            state.pages = pages;
            next_page(leaf, state, options);
        }
        (Node::Struct { children, .. }, State::Struct { children: states, .. }) => {
            for (child, state) in children.iter().zip(states) {
                begin_write(child, state, options);
            }
        }
        (Node::List { child, .. } | Node::Array { child, .. }, State::List { child: state, .. }) => {
            begin_write(child, state, options)
        }
        _ => {}
    }
}

/// `WriteLevels`: RLE, after the length of the run in four bytes.
fn write_levels(out: &mut Vec<u8>, levels: &[u16], max_value: u16, offset: usize, count: usize, no_nulls: bool) {
    if max_value == 0 || levels.is_empty() || count == 0 {
        return;
    }
    let mut encoder = RleBpEncoder::new(bit_width_of_max(u64::from(max_value)));
    let mut runs = Vec::new();
    encoder.begin_write();
    if no_nulls {
        encoder.write_many(&mut runs, u32::from(levels[0]), count);
    } else {
        for &level in &levels[offset..offset + count] {
            encoder.write_value(&mut runs, u32::from(level));
        }
    }
    encoder.finish_write(&mut runs);
    out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
    out.extend_from_slice(&runs);
}

/// `NextPage`: flushes the page before, and starts the next with its levels.
fn next_page(leaf: &Leaf, state: &mut LeafState, options: &WriterOptions) {
    if state.current_page > 0 {
        flush_page(state, options);
    }
    if state.current_page >= state.pages.len() {
        state.current_page = state.pages.len() + 1;
        return;
    }
    let info = state.page_info[state.current_page].clone();
    let no_nulls = state.levels.null_count + state.levels.parent_null_count == 0;
    let page = &mut state.pages[state.current_page];
    state.current_page += 1;
    write_levels(
        &mut page.buffer,
        &state.levels.repetition,
        leaf.schema.max_repeat,
        info.offset,
        info.row_count,
        false,
    );
    write_levels(
        &mut page.buffer,
        &state.levels.definition,
        leaf.schema.max_define,
        info.offset,
        info.row_count,
        no_nulls,
    );
}

/// `FlushPage`: the encoder's last bytes, then compression.
fn flush_page(state: &mut LeafState, options: &WriterOptions) {
    if state.current_page == 0 || state.current_page > state.pages.len() {
        return;
    }
    let page = &mut state.pages[state.current_page - 1];
    flush_encoder(&mut page.encoder, &mut page.buffer);
    // the statistics of the column chunk are the merged statistics of its pages
    if let (Some(page_stats), Some(stats)) = (page.page_stats.as_ref(), state.stats.as_mut()) {
        stats.merge(page_stats);
    }
    page.uncompressed_size = page.buffer.len();
    let data = std::mem::take(&mut page.buffer);
    page.compressed = options.codec.compress(data).unwrap_or_default();
}

/// `FlushPageState`.
fn flush_encoder(encoder: &mut PageEncoder, out: &mut Vec<u8>) {
    match encoder {
        PageEncoder::Plain => {}
        PageEncoder::Dictionary {
            encoder,
            written,
            bit_width,
        } => {
            if !*written {
                out.push(*bit_width as u8);
                return;
            }
            encoder.finish_write(out);
        }
        PageEncoder::Dbp { encoder, initialized } => {
            if !*initialized {
                encoder.begin_write(out, 0);
            }
            encoder.finish_write(out);
        }
        PageEncoder::Dlba { encoder, initialized } => {
            if !*initialized {
                encoder.begin_write(out, b"");
            }
            encoder.finish_write(out);
        }
        PageEncoder::Bss(encoder) => encoder.finish_write(out),
        PageEncoder::Boolean { byte, position } => {
            if *position > 0 {
                out.push(*byte);
                *byte = 0;
                *position = 0;
            }
        }
    }
}

/// `Write`: the values of a vector, page by page.
fn write(node: &Node, state: &mut State, input: &Input, count: usize, options: &WriterOptions) -> Result<()> {
    match (node, state, input) {
        (Node::Leaf(leaf), State::Leaf(state), Input::Leaf(values)) => {
            let mut remaining = count;
            let mut offset = 0;
            while remaining > 0 {
                let page_index = state.current_page - 1;
                let Some(page) = state.pages.get(page_index) else {
                    return Err(io_error("Writes are not correctly aligned!?"));
                };
                let write_count = remaining.min(page.max_write_count - page.write_count);
                write_values(leaf, state, page_index, &values[offset..offset + write_count], options)?;
                let page = &mut state.pages[page_index];
                page.write_count += write_count;
                if page.write_count == page.max_write_count {
                    next_page(leaf, state, options);
                }
                offset += write_count;
                remaining -= write_count;
            }
            Ok(())
        }
        (
            Node::Struct { children, .. },
            State::Struct { children: states, .. },
            Input::Struct { children: inputs, .. },
        ) => {
            for ((child, state), input) in children.iter().zip(states).zip(inputs) {
                write(child, state, input, count, options)?;
            }
            Ok(())
        }
        (
            Node::List { child, .. } | Node::Array { child, .. },
            State::List { child: state, .. },
            Input::List {
                child: input,
                child_count,
                ..
            },
        ) => write(child, state, input, *child_count, options),
        _ => Err(io_error("a Parquet column writer without its state")),
    }
}

/// `WriteVector`: values of a page.
fn write_values(
    leaf: &Leaf,
    state: &mut LeafState,
    page_index: usize,
    values: &[Value],
    options: &WriterOptions,
) -> Result<()> {
    let LeafState {
        pages,
        stats: chunk_stats,
        dictionary,
        enum_seen,
        ..
    } = state;
    let page = &mut pages[page_index];
    let out = &mut page.buffer;
    // with the page index every page tracks its own statistics
    let stats = match page.page_stats.as_mut() {
        Some(stats) => Some(stats),
        None => chunk_stats.as_mut(),
    };
    match &leaf.kind {
        LeafKind::Boolean => {
            let PageEncoder::Boolean { byte, position } = &mut page.encoder else {
                return Ok(());
            };
            let Some(Stats::Bool { min, max }) = stats else {
                return Ok(());
            };
            // like C++, once a page has seen both values the statistics stop changing
            for value in values {
                let ValueData::Bool(flag) = value.data() else {
                    continue;
                };
                *max |= *flag;
                *min &= *flag;
                *byte |= u8::from(*flag) << *position;
                *position += 1;
                if *position == 8 {
                    out.push(*byte);
                    *byte = 0;
                    *position = 0;
                }
            }
        }
        LeafKind::Enum(_) => {
            let PageEncoder::Dictionary {
                encoder,
                written,
                bit_width,
            } = &mut page.encoder
            else {
                return Ok(());
            };
            for value in values {
                let Some(index) = value.as_i128() else {
                    continue;
                };
                if value.is_null() {
                    continue;
                }
                if !*written {
                    out.push(*bit_width as u8);
                    encoder.begin_write();
                    *written = true;
                }
                encoder.write_value(out, index as u32);
                if let Some(seen) = enum_seen.get_mut(index as usize) {
                    *seen = true;
                }
            }
        }
        LeafKind::FixedDecimal => {
            let Some(Stats::Decimal { min, max }) = stats else {
                return Ok(());
            };
            for value in values {
                if value.is_null() {
                    continue;
                }
                let unscaled = value.as_i128().unwrap_or(0);
                if min.is_none_or(|m| unscaled < m) {
                    *min = Some(unscaled);
                }
                if max.is_none_or(|m| unscaled > m) {
                    *max = Some(unscaled);
                }
                out.extend_from_slice(&unscaled.to_be_bytes());
            }
        }
        LeafKind::Standard { convert, .. } => {
            let stats_per_value = !matches!(page.encoder, PageEncoder::Dictionary { .. })
                || matches!(convert, Convert::F32 | Convert::F64)
                || options.write_page_index;
            let mut stats = stats;
            for value in values {
                if value.is_null() {
                    continue;
                }
                let target = convert.apply(value)?;
                if stats_per_value && let Some(stats) = stats.as_deref_mut() {
                    stats.update(&target);
                }
                match &mut page.encoder {
                    PageEncoder::Dictionary {
                        encoder,
                        written,
                        bit_width,
                    } => {
                        if !*written {
                            out.push(*bit_width as u8);
                            encoder.begin_write();
                            *written = true;
                        }
                        let index = dictionary
                            .as_ref()
                            .and_then(|d| d.index.get(&target.key()).copied())
                            .unwrap_or(0);
                        encoder.write_value(out, index);
                    }
                    PageEncoder::Dbp { encoder, initialized } => {
                        if !*initialized {
                            encoder.begin_write(out, target.as_i64());
                            *initialized = true;
                        } else {
                            encoder.write_value(out, target.as_i64());
                        }
                    }
                    PageEncoder::Dlba { encoder, initialized } => {
                        let bytes = target.raw();
                        if !*initialized {
                            encoder.begin_write(out, &bytes);
                            *initialized = true;
                        } else {
                            encoder.write_value(out, &bytes);
                        }
                    }
                    PageEncoder::Bss(encoder) => encoder.write_value(&target.raw()),
                    _ => target.plain(out),
                }
            }
        }
    }
    Ok(())
}

/// `PrepareWrite`: the last page, and the dictionary page in front of the others.
fn prepare_write(node: &Node, state: &mut State, options: &WriterOptions) -> Result<()> {
    match (node, state) {
        (Node::Leaf(leaf), State::Leaf(state)) => {
            flush_page(state, options);
            if has_dictionary(leaf, state) {
                flush_dictionary(leaf, state, options)?;
            }
            Ok(())
        }
        (Node::Struct { children, .. }, State::Struct { children: states, .. }) => {
            for (child, state) in children.iter().zip(states) {
                prepare_write(child, state, options)?;
            }
            Ok(())
        }
        (Node::List { child, .. } | Node::Array { child, .. }, State::List { child: state, .. }) => {
            prepare_write(child, state, options)
        }
        _ => Ok(()),
    }
}

/// `FlushDictionary`: the statistics and the bloom filter of the dictionary's values, and its page.
fn flush_dictionary(leaf: &Leaf, state: &mut LeafState, options: &WriterOptions) -> Result<()> {
    let mut data = Vec::new();
    let count;
    match &leaf.kind {
        LeafKind::Enum(entries) => {
            count = entries.len();
            for (entry, seen) in entries.iter().zip(&state.enum_seen) {
                if *seen {
                    if let Some(stats) = state.stats.as_mut() {
                        stats.update(&Target::Bytes(entry.as_bytes().to_vec()));
                    }
                    data.extend_from_slice(&(entry.len() as u32).to_le_bytes());
                    data.extend_from_slice(entry.as_bytes());
                } else {
                    data.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        LeafKind::Standard { convert, .. } => {
            let dictionary = state.dictionary.as_ref().expect("a dictionary");
            count = dictionary.values.len();
            if options.bloom_filters {
                state.bloom =
                    BloomFilter::create(count as u64 * leaf.bloom_entries_per_value(), options.bloom_filter_fpp);
            }
            let interval = leaf.bloom_entries_per_value() == 2;
            let stats_from_dictionary = !matches!(convert, Convert::F32 | Convert::F64) && !options.write_page_index;
            for value in &dictionary.values {
                if stats_from_dictionary && let Some(stats) = state.stats.as_mut() {
                    stats.update(value);
                }
                if let Some(bloom) = state.bloom.as_mut() {
                    insert_bloom_value(bloom, value, interval);
                }
                value.plain(&mut data);
            }
        }
        _ => return Ok(()),
    }
    let uncompressed_size = data.len();
    let compressed = options.codec.compress(data)?;
    state.pages.insert(
        0,
        PageWrite {
            page_type: DICTIONARY_PAGE,
            num_values: count,
            encoding: enc::PLAIN,
            buffer: Vec::new(),
            uncompressed_size,
            compressed,
            write_count: 0,
            max_write_count: 0,
            encoder: PageEncoder::Plain,
            page_stats: None,
            num_rows: 0,
            null_count: 0,
        },
    );
    Ok(())
}

/// The hash a bloom filter of a column of type `ty` holds for `value` (`ParquetBloomFilterHashStrategy`): of the
/// value as the writer stores it, or for intervals written with `normalized-v1`, of its normalized form. `None`
/// for values no filter holds.
pub fn bloom_probe_hash(ty: &LogicalType, value: &Value, normalized_intervals: bool) -> Result<Option<u64>> {
    if value.is_null() {
        return Ok(None);
    }
    let Node::Leaf(leaf) = create_node(Vec::new(), ty, "probe", 0, 1, true, &WriterOptions::default(), None)? else {
        return Ok(None);
    };
    let LeafKind::Standard { convert, .. } = leaf.kind else {
        return Ok(None);
    };
    let raw = convert.apply(value)?.raw();
    if convert == Convert::Interval && normalized_intervals {
        return Ok(interval_normalized_hash(&raw));
    }
    Ok(Some(xxhash64(&raw)))
}

/// `InitializeStatsState`: empty statistics of a leaf.
fn fresh_stats(leaf: &Leaf) -> Option<Stats> {
    match &leaf.kind {
        LeafKind::Standard { stats, .. } => Stats::new(*stats),
        LeafKind::Boolean => Some(Stats::Bool { min: true, max: false }),
        LeafKind::Enum(_) => Some(Stats::String(StringStats::new(false, true))),
        LeafKind::FixedDecimal => Some(Stats::Decimal { min: None, max: None }),
    }
}

/// The page index of a column chunk (`ParquetPageIndexEntry`).
struct PageIndexEntry {
    column: usize,
    row_group: usize,
    has_column_index: bool,
    null_pages: Vec<bool>,
    min_values: Vec<Vec<u8>>,
    max_values: Vec<Vec<u8>>,
    boundary_order: i32,
    null_counts: Vec<i64>,
    /// offset, compressed size with the header, first row
    locations: Vec<(i64, i32, i64)>,
}

impl PageIndexEntry {
    fn column_index(&self) -> Vec<u8> {
        let mut t = Thrift::new();
        t.begin();
        t.list(1, ttype::BOOL_TRUE, self.null_pages.len());
        for null in &self.null_pages {
            t.out.push(if *null { 1 } else { 2 });
        }
        t.list(2, ttype::BINARY, self.min_values.len());
        for value in &self.min_values {
            t.raw_binary(value);
        }
        t.list(3, ttype::BINARY, self.max_values.len());
        for value in &self.max_values {
            t.raw_binary(value);
        }
        t.i32(4, self.boundary_order);
        t.list(5, ttype::I64, self.null_counts.len());
        for count in &self.null_counts {
            encoding::varint(&mut t.out, encoding::zigzag(*count));
        }
        t.end();
        t.out
    }

    fn offset_index(&self) -> Vec<u8> {
        let mut t = Thrift::new();
        t.begin();
        t.list(1, ttype::STRUCT, self.locations.len());
        for (offset, size, first_row) in &self.locations {
            t.element_struct(|t| {
                t.i64(1, *offset);
                t.i32(2, *size);
                t.i64(3, *first_row);
            });
        }
        t.end();
        t.out
    }
}

/// `FinalizePageIndex`: the ColumnIndex needs bounds for every page with values, and its order.
fn finalize_page_index(pages: &[PageWrite], index: &mut PageIndexEntry) {
    index.has_column_index = true;
    let mut previous: Option<&Stats> = None;
    let (mut ascending, mut descending) = (true, true);
    for (page_number, page) in pages.iter().filter(|page| page.page_type == DATA_PAGE).enumerate() {
        let stats = page.page_stats.as_ref();
        let null_page = index.null_pages[page_number];
        let page_has_values = page.null_count < page.max_write_count;
        if (null_page && page_has_values) || stats.is_some_and(|s| s.nan_count() > 0) {
            index.has_column_index = false;
            return;
        }
        if null_page {
            continue;
        }
        let stats = stats.expect("a page with bounds has statistics");
        if let Some(previous) = previous {
            use std::cmp::Ordering::*;
            let (min, max) = (previous.compare(stats, false), previous.compare(stats, true));
            ascending = ascending && min != Greater && max != Greater;
            descending = descending && min != Less && max != Less;
        }
        previous = Some(stats);
    }
    index.boundary_order = if ascending {
        1
    } else if descending {
        2
    } else {
        0
    };
}
