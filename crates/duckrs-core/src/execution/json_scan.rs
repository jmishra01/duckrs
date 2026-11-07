// Ported from duckdb/extension/json/json_scan.cpp, json_reader.cpp and json_functions/read_json.cpp
// @ rust-port-baseline
//
//! `read_json` and its kin. A file is read whole, one thread scans it, and the columns come from
//! the structure of a sample of its records.

use super::csv::hive_parts;
use super::csv_sniffer::{Parsing, can_cast};
use super::file_system::{Compression, read_file};
use super::physical::{PhysicalOperator, SourceState};
use crate::function::cast::{CastMode, cast_vector};
use crate::function::dates::strptime_micros;
use crate::function::json::{Json, ParseError, Structure, json_type, parse_prefix, transform_with};
use duckrs_common::types::{LogicalType, Timestamp};
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use std::sync::Arc;

/// `JSONFormat`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonFormat {
    Auto,
    Unstructured,
    NewlineDelimited,
    Array,
}

/// `JSONScanData`
#[derive(Debug)]
pub struct JsonScanData {
    pub files: Vec<String>,
    pub format: JsonFormat,
    pub compression: Compression,
    /// each record is an object of the columns, instead of the value of one column
    pub records: bool,
    /// the records as text, as `read_json_objects` gives them
    pub objects: bool,
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub ignore_errors: bool,
    pub case_insensitive: bool,
    pub date_format: Option<String>,
    pub timestamp_format: Option<String>,
    pub filename: bool,
    pub hive: Vec<(String, LogicalType)>,
}

impl JsonScanData {
    pub fn output_types(&self) -> Vec<LogicalType> {
        let mut types = self.types.clone();
        if self.filename {
            types.push(LogicalType::VARCHAR);
        }
        types.extend(self.hive.iter().map(|(_, ty)| ty.clone()));
        types
    }
}

/// A record of a file, or why it isn't one, by the byte where that shows.
pub type Record = std::result::Result<Json, ParseError>;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_space(data: &[u8], mut at: usize) -> usize {
    while data.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    at
}

fn lines_of(data: &[u8]) -> Vec<Record> {
    let mut records = Vec::new();
    let mut start = 0;
    while start < data.len() {
        let end = data[start..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(data.len(), |i| start + i);
        let first = skip_space(&data[..end], start);
        if first < end {
            records.push(match parse_prefix(&data[..end], first) {
                Ok((value, stop)) if skip_space(&data[..end], stop) == end => Ok(value),
                Ok((_, stop)) => Err(ParseError {
                    position: stop,
                    message: "unexpected content after document",
                }),
                Err(error) => Err(error),
            });
        }
        start = end + 1;
    }
    records
}

fn concatenated(data: &[u8]) -> Vec<Record> {
    let mut records = Vec::new();
    let mut at = skip_space(data, 0);
    while at < data.len() {
        match parse_prefix(data, at) {
            Ok((value, end)) => {
                records.push(Ok(value));
                at = skip_space(data, end);
                // a comma between values is fine in C++'s unstructured format
                if data.get(at) == Some(&b',') {
                    at = skip_space(data, at + 1);
                }
            }
            Err(error) => {
                records.push(Err(error));
                break;
            }
        }
    }
    records
}

fn array_items(data: &[u8]) -> Vec<Record> {
    let start = skip_space(data, 0);
    if start == data.len() {
        return Vec::new();
    }
    match parse_prefix(data, start) {
        Ok((Json::Array(items), end)) if skip_space(data, end) == data.len() => items.into_iter().map(Ok).collect(),
        Ok(_) => vec![Err(ParseError {
            position: start,
            message: "expected top-level JSON array",
        })],
        Err(error) => vec![Err(error)],
    }
}

/// The records of a file, and the format they turned out to have.
pub fn read_records(data: &[u8], format: JsonFormat) -> (Vec<Record>, JsonFormat) {
    let data = data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data);
    match format {
        JsonFormat::Array => (array_items(data), JsonFormat::Array),
        JsonFormat::NewlineDelimited => (lines_of(data), JsonFormat::NewlineDelimited),
        JsonFormat::Unstructured => (concatenated(data), JsonFormat::Unstructured),
        JsonFormat::Auto => {
            let first = skip_space(data, 0);
            if data.get(first) == Some(&b'[') {
                let items = array_items(data);
                if items.iter().all(Record::is_ok) {
                    return (items, JsonFormat::Array);
                }
            }
            let lines = lines_of(data);
            if lines.iter().all(Record::is_ok) {
                return (lines, JsonFormat::NewlineDelimited);
            }
            (concatenated(data), JsonFormat::Unstructured)
        }
    }
}

/// `JSONReader::ThrowParseError`
pub fn record_error(path: &str, data: &[u8], error: &ParseError) -> Error {
    let line = data[..error.position.min(data.len())]
        .iter()
        .filter(|&&c| c == b'\n')
        .count()
        + 1;
    Error::new(
        ErrorKind::InvalidInput,
        format!(
            "Malformed JSON in file \"{path}\", at byte {} in line {line}: {}.",
            error.position + 1,
            error.message
        ),
    )
}

/// `JSONStructure` of the records, with what strings turn out to hold.
pub fn infer(records: &[&Json]) -> Structure {
    let mut structure = Structure::Null;
    for record in records {
        structure = structure.merge(Structure::of(record));
    }
    structure
}

/// A string that is a date, a timestamp or a UUID everywhere: `JSONSchema::TryStringToType`.
pub fn refine_type(ty: LogicalType, values: &[&Json], formats: &Formats) -> LogicalType {
    use duckrs_common::types::LogicalTypeId as Id;
    let parts = |key: &str| -> Vec<&Json> {
        values
            .iter()
            .filter_map(|value| match value {
                Json::Object(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            })
            .collect()
    };
    match ty.id() {
        Id::Varchar if ty.alias().is_none() => {
            let texts: Vec<&str> = values
                .iter()
                .filter_map(|value| match value {
                    Json::Str(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if texts.is_empty() || texts.len() != values.iter().filter(|v| !matches!(v, Json::Null)).count() {
                return ty;
            }
            let zoned = LogicalType::new(Id::TimestampTz);
            for candidate in [LogicalType::DATE, LogicalType::TIMESTAMP, zoned, LogicalType::UUID] {
                if texts.iter().all(|text| string_fits(text, &candidate, formats)) {
                    return candidate;
                }
            }
            ty
        }
        Id::BigInt => {
            if values
                .iter()
                .any(|value| matches!(value, Json::UInt(x) if *x > i64::MAX as u64))
            {
                LogicalType::HUGEINT
            } else {
                ty
            }
        }
        Id::Struct => {
            let children: Vec<(String, LogicalType)> = ty
                .struct_children()
                .iter()
                .map(|(name, child)| (name.clone(), refine_type(child.clone(), &parts(name), formats)))
                .collect();
            if formats
                .map_threshold
                .is_some_and(|threshold| children.len() > threshold)
            {
                let first = children[0].1.clone();
                let same = children.iter().all(|(_, child)| *child == first);
                return LogicalType::map(LogicalType::VARCHAR, if same { first } else { json_type() });
            }
            LogicalType::struct_type(children)
        }
        Id::List => {
            let items: Vec<&Json> = values
                .iter()
                .filter_map(|value| match value {
                    Json::Array(items) => Some(items.iter()),
                    _ => None,
                })
                .flatten()
                .collect();
            let child = ty.child_type().cloned().unwrap_or(LogicalType::VARCHAR);
            LogicalType::list(refine_type(child, &items, formats))
        }
        _ => ty,
    }
}

/// How values of a file become columns.
#[derive(Debug, Clone, Default)]
pub struct Formats {
    pub date: Option<String>,
    pub timestamp: Option<String>,
    pub missing_keys_are_null: bool,
    pub case_insensitive: bool,
    /// objects with more keys than this are MAPs (`map_inference_threshold`); `None` turns that off
    pub map_threshold: Option<usize>,
}

fn string_fits(text: &str, ty: &LogicalType, formats: &Formats) -> bool {
    let format = match ty.id() {
        duckrs_common::types::LogicalTypeId::Date => formats.date.as_deref(),
        duckrs_common::types::LogicalTypeId::Timestamp => formats.timestamp.as_deref(),
        _ => None,
    };
    if let Some(format) = format {
        return strptime_micros(text, format).is_ok();
    }
    let parsing = Parsing {
        decimal_separator: b'.',
        ..Parsing::default()
    };
    can_cast(text, ty, &parsing)
}

/// What a string with a format of its own is.
pub fn formatted_value(text: &str, ty: &LogicalType, formats: &Formats) -> Option<Value> {
    use duckrs_common::types::LogicalTypeId as Id;
    match ty.id() {
        Id::Date => {
            let micros = strptime_micros(text, formats.date.as_deref()?).ok()?;
            Some(Value::date(Timestamp(micros).date()))
        }
        Id::Timestamp => {
            let micros = strptime_micros(text, formats.timestamp.as_deref()?).ok()?;
            Some(Value::timestamp(Timestamp(micros)))
        }
        _ => None,
    }
}

/// How far a scan got.
#[derive(Debug, Default)]
pub struct JsonScanState {
    file: usize,
    data: Vec<u8>,
    records: Option<std::vec::IntoIter<Record>>,
}

fn record_row(scan: &JsonScanData, record: &Json, formats: &Formats, strict: bool) -> Result<Option<Vec<Value>>> {
    if scan.objects {
        return Value::varchar(record.text())
            .reinterpret(&json_type())
            .map(|v| Some(vec![v]));
    }
    if !scan.records {
        let value = transform_with(record, &scan.types[0], strict, formats)?;
        return Ok(Some(vec![value]));
    }
    let Json::Object(items) = record else {
        if scan.ignore_errors {
            return Ok(None);
        }
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Expected OBJECT, but got {}: {}\nTry increasing \"sample_size\" or setting \"records\" to false",
                record.kind_name(),
                record.text()
            ),
        ));
    };
    let mut row = Vec::with_capacity(scan.names.len());
    for (name, ty) in scan.names.iter().zip(&scan.types) {
        let found = if formats.case_insensitive {
            items.iter().find(|(k, _)| k.eq_ignore_ascii_case(name))
        } else {
            items.iter().find(|(k, _)| k == name)
        };
        let value = match found {
            Some((_, item)) => match transform_with(item, ty, strict, formats) {
                Ok(value) => value,
                Err(_) if scan.ignore_errors => return Ok(None),
                Err(error) => return Err(error),
            },
            None => Value::null(ty.clone()),
        };
        row.push(value);
    }
    Ok(Some(row))
}

fn extra_columns(scan: &JsonScanData, path: &str, count: usize, columns: &mut Vec<Vector>) -> Result<()> {
    let constant = |ty: &LogicalType, value: &Value| -> Result<Vector> {
        let mut column = Vector::new(ty.clone(), count);
        for row in 0..count {
            column.set_value(row, value)?;
        }
        Ok(column)
    };
    if scan.filename {
        columns.push(constant(&LogicalType::VARCHAR, &Value::varchar(path))?);
    }
    if !scan.hive.is_empty() {
        let parts = hive_parts(path);
        for (key, ty) in &scan.hive {
            let value = parts
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .and_then(|(_, v)| v.clone());
            let value = value.map_or(Value::null(LogicalType::VARCHAR), Value::varchar);
            let strings = constant(&LogicalType::VARCHAR, &value)?;
            let mut typed = Vector::new(ty.clone(), count);
            cast_vector(&strings, &mut typed, count, CastMode::Strict)?;
            columns.push(typed);
        }
    }
    Ok(())
}

/// The `read_json` table function.
#[derive(Debug)]
pub struct PhysicalJsonScan {
    pub types: Vec<LogicalType>,
    pub scan: Arc<JsonScanData>,
}

impl PhysicalOperator for PhysicalJsonScan {
    fn name(&self) -> &'static str {
        "READ_JSON"
    }
    fn types(&self) -> &[LogicalType] {
        &self.types
    }
    fn is_source(&self) -> bool {
        true
    }
    fn source_state(&self) -> SourceState {
        Box::new(JsonScanState::default())
    }
    fn get_data(&self, state: &mut SourceState) -> Result<Option<DataChunk>> {
        let state: &mut JsonScanState = state.downcast_mut().expect("json scan state");
        let scan = &self.scan;
        let formats = Formats {
            date: scan.date_format.clone(),
            timestamp: scan.timestamp_format.clone(),
            missing_keys_are_null: true,
            case_insensitive: scan.case_insensitive,
            map_threshold: None,
        };
        let strict = !scan.ignore_errors;
        loop {
            let Some(path) = scan.files.get(state.file) else {
                return Ok(None);
            };
            if state.records.is_none() {
                state.data = read_file(path, scan.compression)?;
                let (records, _) = read_records(&state.data, scan.format);
                state.records = Some(records.into_iter());
            }
            let records = state.records.as_mut().expect("just read");
            let mut rows = Vec::with_capacity(STANDARD_VECTOR_SIZE);
            while rows.len() < STANDARD_VECTOR_SIZE {
                match records.next() {
                    None => break,
                    Some(Ok(record)) => {
                        if let Some(row) = record_row(scan, &record, &formats, strict)? {
                            rows.push(row);
                        }
                    }
                    Some(Err(_)) if scan.ignore_errors => {}
                    Some(Err(error)) => return Err(record_error(path, &state.data, &error)),
                }
            }
            if rows.is_empty() {
                state.records = None;
                state.file += 1;
                continue;
            }
            let count = rows.len();
            let mut columns = Vec::with_capacity(scan.types.len() + 1 + scan.hive.len());
            for (index, ty) in scan.types.iter().enumerate() {
                let mut column = Vector::new(ty.clone(), count);
                for (row, values) in rows.iter().enumerate() {
                    column.set_value(row, &values[index])?;
                }
                columns.push(column);
            }
            extra_columns(scan, path, count, &mut columns)?;
            return Ok(Some(DataChunk::from_columns(columns, count)));
        }
    }
}
