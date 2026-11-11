// Ported from duckdb/extension/json/json_scan.cpp, json_reader.cpp and json_functions/read_json.cpp
// @ rust-port-baseline
//
//! `read_json` and its kin. A file is read whole, one thread scans it, and the columns come from
//! the structure of a sample of its records.

use super::csv_sniffer::{Parsing, can_cast};
use super::file_system::{Compression, read_file};
use super::physical::{PhysicalOperator, SourceState};
use crate::function::dates::strptime_micros;
use crate::function::json::{Json, ParseError, Structure, json_type, parse_prefix_with, transform_with};
use duckrs_common::types::{LogicalType, Timestamp};
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result, STANDARD_VECTOR_SIZE};
use std::sync::Arc;

/// `JSONFormat`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JsonFormat {
    #[default]
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
    /// for the hint of a transform error
    pub auto_detect: bool,
    pub case_insensitive: bool,
    pub date_format: Option<String>,
    pub timestamp_format: Option<String>,
    pub filename: bool,
    pub hive: Vec<(String, LogicalType)>,
    /// the virtual columns, after the rest
    pub virtual_columns: Vec<crate::execution::multi_file::VirtualColumn>,
}

impl JsonScanData {
    pub fn output_types(&self) -> Vec<LogicalType> {
        let mut types = self.types.clone();
        if self.filename {
            types.push(LogicalType::VARCHAR);
        }
        types.extend(self.hive.iter().map(|(_, ty)| ty.clone()));
        types.extend(crate::execution::multi_file::virtual_types(&self.virtual_columns));
        types
    }
}

/// A record of a file and the bytes it is (the unit `read_json_objects` gives), or why it isn't
/// one, by the byte where that shows.
pub type Record = std::result::Result<(Json, std::ops::Range<usize>), ParseError>;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_space(data: &[u8], mut at: usize) -> usize {
    while data.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    at
}

fn lines_of(data: &[u8], raw: bool) -> Vec<Record> {
    let mut records = Vec::new();
    let mut start = 0;
    while start < data.len() {
        let end = data[start..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(data.len(), |i| start + i);
        let first = skip_space(&data[..end], start);
        if first < end {
            records.push(match parse_prefix_with(&data[..end], first, raw) {
                Ok((value, stop)) if skip_space(&data[..end], stop) == end => Ok((value, first..stop)),
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

fn concatenated(data: &[u8], raw: bool) -> Vec<Record> {
    let mut records = Vec::new();
    let mut at = skip_space(data, 0);
    while at < data.len() {
        match parse_prefix_with(data, at, raw) {
            Ok((value, end)) => {
                records.push(Ok((value, at..end)));
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

/// The errors of `JSONReader`'s array format, which are of the file and not of a record.
const NOT_AN_ARRAY: &str = "expected top-level JSON array";
const NO_CLOSING_BRACKET: &str = "missing closing brace";
const TRAILING_DATA: &str = "empty array with trailing data";

fn array_items(data: &[u8], raw: bool) -> Vec<Record> {
    let start = skip_space(data, 0);
    if start == data.len() {
        return Vec::new();
    }
    let not_an_array = || {
        vec![Err(ParseError {
            position: start,
            message: NOT_AN_ARRAY,
        })]
    };
    if data[start] != b'[' {
        return not_an_array();
    }
    // each item on its own, for the bytes it is
    let mut records = Vec::new();
    let mut at = skip_space(data, start + 1);
    if at == data.len() {
        return vec![Err(ParseError {
            position: at,
            message: NO_CLOSING_BRACKET,
        })];
    }
    if data[at] == b']' {
        if skip_space(data, at + 1) != data.len() {
            return vec![Err(ParseError {
                position: at,
                message: TRAILING_DATA,
            })];
        }
        return records;
    }
    loop {
        match data.get(at) {
            Some(b']') => break,
            None => {
                return vec![Err(ParseError {
                    position: at,
                    message: "unexpected end of data",
                })];
            }
            _ => {}
        }
        let end = match parse_prefix_with(data, at, raw) {
            Ok((value, end)) => {
                records.push(Ok((value, at..end)));
                end
            }
            Err(error) => return vec![Err(error)],
        };
        at = skip_space(data, end);
        match data.get(at) {
            Some(b',') => at = skip_space(data, at + 1),
            Some(b']') => break,
            None => {
                return vec![Err(ParseError {
                    position: at,
                    message: "unexpected end of data",
                })];
            }
            _ => {
                return vec![Err(ParseError {
                    position: at,
                    message: "unexpected character, expected a comma or a closing bracket",
                })];
            }
        }
    }
    if skip_space(data, at + 1) != data.len() {
        return not_an_array();
    }
    records
}

/// The records of a file, and the format they turned out to have.
pub fn read_records(data: &[u8], format: JsonFormat) -> (Vec<Record>, JsonFormat) {
    read_records_with(data, format, false)
}

/// `read_records`, with `raw` every number kept as its text (`JSONCommon::NumbersAsRaw`).
pub fn read_records_with(data: &[u8], format: JsonFormat, raw: bool) -> (Vec<Record>, JsonFormat) {
    let bom = data.starts_with(&[0xef, 0xbb, 0xbf]);
    let (mut records, format) = records_of(if bom { &data[3..] } else { data }, format, raw);
    if bom {
        for record in &mut records {
            match record {
                Ok((_, span)) => *span = span.start + 3..span.end + 3,
                Err(error) => error.position += 3,
            }
        }
    }
    (records, format)
}

fn records_of(data: &[u8], format: JsonFormat, raw: bool) -> (Vec<Record>, JsonFormat) {
    match format {
        JsonFormat::Array => (array_items(data, raw), JsonFormat::Array),
        JsonFormat::NewlineDelimited => (lines_of(data, raw), JsonFormat::NewlineDelimited),
        JsonFormat::Unstructured => (concatenated(data, raw), JsonFormat::Unstructured),
        JsonFormat::Auto => match detect_format(data) {
            JsonFormat::Array => (array_items(data, raw), JsonFormat::Array),
            JsonFormat::NewlineDelimited => (lines_of(data, raw), JsonFormat::NewlineDelimited),
            _ => (concatenated(data, raw), JsonFormat::Unstructured),
        },
    }
}

/// `DetectFormatAndRecordType`, of the format: a first line that is a document makes the file
/// newline-delimited (or an array, if it is all of it); else it is an array if it starts as one.
fn detect_format(data: &[u8]) -> JsonFormat {
    let first = skip_space(data, 0);
    if let Some(newline) = data.iter().position(|&c| c == b'\n') {
        let line_size = skip_space(data, newline);
        if let Ok((json, end)) = parse_prefix_with(&data[..line_size], first, false)
            && skip_space(&data[..line_size], end) == line_size
        {
            return if matches!(json, Json::Array(_)) && line_size == data.len() {
                JsonFormat::Array
            } else {
                JsonFormat::NewlineDelimited
            };
        }
    }
    match data.get(first) {
        None | Some(b'{') => JsonFormat::Unstructured,
        Some(b'[') => match parse_prefix_with(data, first, false) {
            // more than one top-level array is not an array
            Ok((_, end)) if skip_space(data, end) != data.len() => JsonFormat::Unstructured,
            _ => JsonFormat::Array,
        },
        Some(_) => JsonFormat::Unstructured,
    }
}

/// `JSONReader::ThrowParseError`
pub fn record_error(path: &str, data: &[u8], error: &ParseError) -> Error {
    let file_error = |message: String| Error::new(ErrorKind::InvalidInput, message);
    match error.message {
        NOT_AN_ARRAY => {
            let first = data.get(error.position).map_or(' ', |&c| c as char);
            return file_error(format!(
                "Expected top-level JSON array with format='array', but first character is '{first}' in file \"{path}\".\n Try setting format='auto' or format='newline_delimited'."
            ));
        }
        NO_CLOSING_BRACKET => {
            return file_error(format!(
                "Missing closing brace ']' in JSON array with format='array' in file \"{path}\""
            ));
        }
        TRAILING_DATA => {
            return file_error(format!(
                "Empty array with trailing data when parsing JSON array with format='array' in file \"{path}\""
            ));
        }
        _ => {}
    }
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
    infer_with(records, false)
}

/// `infer`, with `case_insensitive_keys`.
pub fn infer_with(records: &[&Json], case_insensitive: bool) -> Structure {
    let mut structure = Structure::Null;
    for record in records {
        structure = structure.merge_with(Structure::of_with(record, case_insensitive), case_insensitive);
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
    /// `error_duplicate_key`: an object with a key twice is an error (the scan's, unless it ignores errors)
    pub error_duplicate_key: bool,
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
    // `DefaultTryCast(.., strict)`: a strict UUID is 32 digits and 4 hyphens (`BaseUUID::FromString`)
    if ty.id() == duckrs_common::types::LogicalTypeId::Uuid {
        let bytes = text.as_bytes();
        if bytes.len() != 36 || [8, 13, 18, 23].iter().any(|&at| bytes[at] != b'-') {
            return false;
        }
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
    /// the format the file turned out to have, and how many of its records were read
    format: JsonFormat,
    record: usize,
    virtual_rows: crate::execution::multi_file::VirtualRows,
}

/// `JSONCommon::ValToString`: a value written, cut after `max_len` bytes.
fn val_to_string(value: &Json, max_len: usize) -> String {
    let text = value.text();
    if text.len() <= max_len {
        return text;
    }
    let mut end = max_len;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// `JSONReader::AddTransformError`: where in which file a record failed, with what to try.
fn transform_error(scan: &JsonScanData, path: &str, format: JsonFormat, record: usize, error: Error) -> Error {
    if error.kind != ErrorKind::InvalidInput && error.kind != ErrorKind::Conversion {
        return error;
    }
    let unit = if format == JsonFormat::NewlineDelimited {
        "line"
    } else {
        "record/value"
    };
    let hint = if scan.auto_detect {
        "\nTry increasing 'sample_size', reducing 'maximum_depth', specifying 'columns', 'format' or 'records' \
         manually, setting 'ignore_errors' to true, or setting 'union_by_name' to true when reading multiple files \
         with a different structure."
    } else {
        "\nTry setting 'auto_detect' to true, specifying 'format' or 'records' manually, or setting 'ignore_errors' \
         to true."
    };
    Error::new(
        ErrorKind::InvalidInput,
        format!(
            "JSON transform error in file \"{path}\", in {unit} {record}: {}{hint}",
            error.message
        ),
    )
}

fn record_row(
    scan: &JsonScanData,
    record: &Json,
    text: &[u8],
    formats: &Formats,
    strict: bool,
) -> Result<Option<Vec<Value>>> {
    // `ReadJSONObjectsFunction`: the bytes of the record as they are
    if scan.objects {
        return Value::varchar(String::from_utf8_lossy(text).into_owned())
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
                "Expected OBJECT, but got {}: {}",
                record.kind_name(),
                val_to_string(record, 50)
            ),
        ));
    };
    let mut row = Vec::with_capacity(scan.names.len());
    for (name, ty) in scan.names.iter().zip(&scan.types) {
        let found = crate::function::json::object_member(record, items, name, formats)?;
        let value = match found {
            Some(item) => match transform_with(item, ty, strict, formats) {
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
        for (key, ty) in &scan.hive {
            let value = super::csv::hive_value(path, key, ty)?;
            columns.push(constant(ty, &value)?);
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
            error_duplicate_key: !scan.ignore_errors,
            map_threshold: None,
        };
        let strict = !scan.ignore_errors;
        loop {
            let Some(path) = scan.files.get(state.file) else {
                return Ok(None);
            };
            if state.records.is_none() {
                state.data = read_file(path, scan.compression)?;
                // `JSONCommon::NumbersAsRaw`: a DECIMAL reads numbers from their text
                let raw = scan.types.iter().any(crate::function::json::contains_decimal);
                let (records, format) = read_records_with(&state.data, scan.format, raw);
                state.records = Some(records.into_iter());
                state.format = format;
                state.record = 0;
            }
            let records = state.records.as_mut().expect("just read");
            let mut rows = Vec::with_capacity(STANDARD_VECTOR_SIZE);
            while rows.len() < STANDARD_VECTOR_SIZE {
                match records.next() {
                    None => break,
                    Some(Ok((record, span))) => {
                        state.record += 1;
                        let text = &state.data[span];
                        let row = record_row(scan, &record, text, &formats, strict)
                            .map_err(|error| transform_error(scan, path, state.format, state.record, error))?;
                        if let Some(row) = row {
                            rows.push(row);
                        }
                    }
                    // a record that isn't JSON is a row of NULLs (the NULL value it is read as)
                    // only lines can be skipped: elsewhere nothing says where the next record starts
                    Some(Err(_)) if scan.ignore_errors && state.format == JsonFormat::NewlineDelimited => {
                        state.record += 1;
                        rows.push(scan.types.iter().map(|ty| Value::null(ty.clone())).collect());
                    }
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
            return crate::execution::multi_file::append_virtual_columns(
                DataChunk::from_columns(columns, count),
                &scan.virtual_columns,
                path,
                state.file,
                &mut state.virtual_rows,
            )
            .map(Some);
        }
    }
}
