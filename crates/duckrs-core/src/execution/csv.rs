// Ported from duckdb/src/execution/operator/csv_scanner/ and duckdb/src/common/csv_writer.cpp
// @ rust-port-baseline
//
//! Reading and writing CSV. A file is read whole and one thread scans it, a row at a time, through
//! C++'s state machine (`csv_state_machine`). The sniffer is in `csv_sniffer`.

use super::csv_sniffer::{Parsing, can_cast, line_size_error, numeric_text, sniff};
pub use super::csv_state_machine::Dialect;
use super::csv_state_machine::{Field, Machine, NewLine, Scanned, ValueScanner, skip_rows};
use super::file_system::{Compression, read_file};
use crate::catalog::TableCatalogEntry;
use crate::function::cast::{CastMode, cast_vector};
use crate::function::dates::{strftime_text, strptime_micros};
use crate::function::json::{Json, value_to_json};
use duckrs_common::types::{LogicalType, LogicalTypeId, Timestamp};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use std::sync::Arc;

const CHUNK_ROWS: usize = duckrs_common::STANDARD_VECTOR_SIZE;

/// What `read_csv` was told (`CSVReaderOptions`).
#[derive(Debug, Clone)]
pub struct CsvReadOptions {
    pub delimiter: Option<Vec<u8>>,
    pub quote: Option<Option<u8>>,
    pub escape: Option<Option<u8>>,
    pub comment: Option<Option<u8>>,
    pub header: Option<bool>,
    pub skip: Option<usize>,
    pub new_line: Option<NewLine>,
    pub strict_mode: bool,
    pub date_format: Option<String>,
    pub timestamp_format: Option<String>,
    pub decimal_separator: u8,
    pub thousands: Option<u8>,
    pub normalize_names: bool,
    pub auto_type_candidates: Option<Vec<LogicalType>>,
    pub force_not_null: Vec<String>,
    pub encoding: Option<String>,
    pub max_line_size: Option<usize>,
    pub buffer_size: Option<usize>,
    pub null_strs: Vec<String>,
    pub auto_detect: bool,
    pub all_varchar: bool,
    pub ignore_errors: bool,
    pub null_padding: bool,
    pub allow_quoted_nulls: bool,
    pub sample_size: usize,
    pub compression: Compression,
}

impl Default for CsvReadOptions {
    fn default() -> Self {
        Self {
            delimiter: None,
            quote: None,
            escape: None,
            comment: None,
            header: None,
            skip: None,
            new_line: None,
            strict_mode: true,
            date_format: None,
            timestamp_format: None,
            decimal_separator: b'.',
            thousands: None,
            normalize_names: false,
            auto_type_candidates: None,
            force_not_null: Vec::new(),
            encoding: None,
            max_line_size: None,
            buffer_size: None,
            null_strs: vec![String::new()],
            auto_detect: true,
            all_varchar: false,
            ignore_errors: false,
            null_padding: false,
            allow_quoted_nulls: true,
            sample_size: 20480,
            compression: Compression::Auto,
        }
    }
}

impl CsvReadOptions {
    /// `maximum_line_size`: 2,000,000 bytes, or what a buffer the user sized holds.
    pub fn maximum_line_size(&self) -> usize {
        self.max_line_size
            .unwrap_or_else(|| self.buffer_size.unwrap_or(2_000_000))
    }

    /// `sample_size_chunks`, of 2048 rows each.
    pub fn sample_chunks(&self) -> usize {
        self.sample_size.div_ceil(2048).max(1)
    }
}

/// `CSVEncoder`: a file, in UTF-8.
pub fn read_csv_file(path: &str, options: &CsvReadOptions) -> Result<Vec<u8>> {
    let data = read_file(path, options.compression)?;
    let encoding = options.encoding.as_deref().unwrap_or("utf-8").to_lowercase();
    match encoding.as_str() {
        "utf-8" | "utf8" => Ok(data),
        "latin-1" | "latin1" => {
            let text: String = data.iter().map(|&b| char::from(b)).collect();
            Ok(text.into_bytes())
        }
        "utf-16" | "utf16" => {
            if data.len() % 2 != 0 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("File is not utf-16 encoded: {path}"),
                ));
            }
            let big_endian = data.starts_with(&[0xfe, 0xff]);
            let units = data
                .chunks_exact(2)
                .map(|c| {
                    if big_endian {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .skip(usize::from(big_endian || data.starts_with(&[0xff, 0xfe])));
            let text: std::result::Result<String, _> = char::decode_utf16(units).collect();
            let text =
                text.map_err(|_| Error::new(ErrorKind::InvalidInput, format!("File is not utf-16 encoded: {path}")))?;
            Ok(text.into_bytes())
        }
        other => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("The CSV Reader does not support the encoding: \"{other}\"\n"),
        )),
    }
}

/// The `key=value` directories of a path (`HivePartitioning::Parse`).
pub fn hive_parts(path: &str) -> Vec<(String, Option<String>)> {
    let mut result = Vec::new();
    let mut parts: Vec<&str> = path.split(['/', '\\']).collect();
    parts.pop();
    for part in parts {
        if let Some((key, value)) = part.split_once('=')
            && !key.is_empty()
            && !value.contains('=')
            && !result.iter().any(|(k, _): &(String, _)| k == key)
        {
            let value = (value != "__HIVE_DEFAULT_PARTITION__").then_some(value.to_string());
            result.push((key.to_string(), value));
        }
    }
    result
}

/// `HivePartitioning::Unescape`: `%XX` escapes decoded.
fn hive_unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            result.push(byte);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// `HivePartitioning::GetValue`: the value of the hive partition `key` of the file at `path`, as `ty`.
pub fn hive_value(path: &str, key: &str, ty: &LogicalType) -> Result<Value> {
    let parts = hive_parts(path);
    let Some(raw) = parts
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.clone())
    else {
        return Ok(Value::null(ty.clone()));
    };
    if ty.id() == LogicalTypeId::Varchar {
        return Ok(Value::varchar(hive_unescape(&raw)));
    }
    if raw.eq_ignore_ascii_case("NULL") || raw.is_empty() {
        return Ok(Value::null(ty.clone()));
    }
    let text = Value::varchar(hive_unescape(&raw));
    crate::execution::parquet::try_cast_value(&text, ty).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Unable to cast '{}' (from hive partition column '{}') to: '{ty}'",
                hive_unescape(&raw),
                key.to_uppercase()
            ),
        )
    })
}

/// A file of a scan, with where its columns go.
#[derive(Debug, Clone)]
pub struct CsvFile {
    pub path: String,
    pub dialect: Dialect,
    pub header: bool,
    /// the lines before its data, without a header (`skip_rows`)
    pub skip: usize,
    /// the lines before its header or first row
    pub rows_until_header: usize,
    /// how many columns its rows have
    pub columns: usize,
    /// for each column of the scan, the column of this file (`union_by_name` leaves gaps)
    pub mapping: Vec<Option<usize>>,
}

/// What the binder found out, and what the scan needs.
#[derive(Debug)]
pub struct CsvScanData {
    pub files: Vec<CsvFile>,
    pub options: CsvReadOptions,
    /// the columns that come from the files
    pub names: Vec<String>,
    pub types: Vec<LogicalType>,
    pub parsing: Parsing,
    /// the columns that come from the directories of the files, after the rest
    pub hive: Vec<(String, LogicalType)>,
    /// the columns where NULL is the empty string
    pub force_not_null: Vec<bool>,
    /// whether the user set a column's type (`was_type_manually_set`)
    pub manual_types: Vec<bool>,
    /// the columns were given, so files aren't sniffed for them
    pub columns_set: bool,
    /// what sniffing found against what the user said
    pub mismatch: String,
    pub union_by_name: bool,
    /// whether a last column says which file a row is from (the `filename` option)
    pub filename: bool,
    /// the virtual columns, after the rest
    pub virtual_columns: Vec<crate::execution::multi_file::VirtualColumn>,
    /// where rejected rows go (`store_rejects`)
    pub rejects: Option<CsvRejects>,
    /// the bytes of the first file that sniffing read, for the first scan (the CSV reader keeps
    /// the handle it sniffed with)
    pub first_data: std::sync::Mutex<Option<Vec<u8>>>,
}

/// The tables that `store_rejects` fills (`CSVRejectsTable`).
#[derive(Debug, Clone)]
pub struct CsvRejects {
    pub errors: Arc<TableCatalogEntry>,
    pub scans: Arc<TableCatalogEntry>,
    pub limit: usize,
    /// how much of a line to keep, 0 for all of it
    pub line_size_limit: usize,
    pub scan_id: u64,
    pub user_arguments: Option<String>,
}

/// `CSVError`, of a row that a scan skipped.
#[derive(Debug)]
struct Reject {
    file: usize,
    line: usize,
    row_position: usize,
    position: Option<usize>,
    column: Option<usize>,
    column_name: Option<String>,
    kind: &'static str,
    text: String,
    message: String,
}

fn count_lines(bytes: &[u8]) -> usize {
    let mut count = 0;
    for (i, &c) in bytes.iter().enumerate() {
        count += usize::from(c == b'\n' || (c == b'\r' && bytes.get(i + 1) != Some(&b'\n')));
    }
    count
}

impl CsvScanData {
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

/// `Is this one of the values that mean NULL`, for the field of a row.
pub fn is_null(field: &Field<'_>, options: &CsvReadOptions, dialect: &Dialect) -> bool {
    if field.quoted && !options.allow_quoted_nulls {
        return false;
    }
    // `\\N` with strict mode off: what is left of it is one byte
    let unquoted_escaped = !dialect.strict && field.escaped && !field.quoted && field.text.len() == 1;
    options.null_strs.iter().any(|null| {
        let null = null.as_bytes();
        if null.len() == 2 && dialect.escape == Some(null[0]) {
            unquoted_escaped && null[1] == field.text[0]
        } else {
            !unquoted_escaped && null == &*field.text
        }
    })
}

fn varchar_vector(fields: &[Option<&Field<'_>>], options: &CsvReadOptions, dialect: &Dialect) -> Result<Vector> {
    let mut result = Vector::new(LogicalType::VARCHAR, fields.len());
    for (i, field) in fields.iter().enumerate() {
        let value = match field {
            Some(field) if !is_null(field, options, dialect) => {
                let text = std::str::from_utf8(&field.text).map_err(|_| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "Invalid unicode (byte sequence mismatch) detected in CSV file",
                    )
                })?;
                Value::varchar(text)
            }
            _ => Value::null(LogicalType::VARCHAR),
        };
        result.set_value(i, &value)?;
    }
    Ok(result)
}

/// `NewLineIdentifierToString`, of what the file uses.
pub fn new_line_identifier(dialect: &Dialect) -> &'static str {
    dialect.new_line.shown()
}

/// How far a scan got (`CSVGlobalState`).
#[derive(Debug, Default)]
pub struct CsvScanState {
    file: usize,
    data: Option<Vec<u8>>,
    scanner: Option<ValueScanner>,
    /// the file being read, if sniffing it found other than the first
    current: Option<CsvFile>,
    line: usize,
    rejects: Vec<Reject>,
    flushed: bool,
    virtual_rows: crate::execution::multi_file::VirtualRows,
}

/// `CSVFileScan`: a file after the first, as sniffing it finds it, with its columns found by name.
fn sniff_next_file(scan: &CsvScanData, file: &CsvFile, data: &[u8]) -> Result<CsvFile> {
    let first = &scan.files[0];
    let as_first = || CsvFile {
        path: file.path.clone(),
        dialect: first.dialect.clone(),
        header: first.header,
        skip: first.skip,
        rows_until_header: first.rows_until_header,
        columns: first.columns,
        mapping: first.mapping.clone(),
    };
    // an empty file is not sniffed: it has the schema of the scan, and no rows
    if data.is_empty() {
        return Ok(as_first());
    }
    let columns = scan
        .columns_set
        .then_some((scan.names.as_slice(), scan.types.as_slice()));
    let sniffed = match sniff(data, &file.path, &scan.options, columns) {
        Ok(sniffed) => sniffed,
        Err(_) if scan.options.ignore_errors => return Ok(as_first()),
        Err(error) => return Err(error),
    };
    let same_shape = sniffed.names.len() == scan.names.len();
    let by_name: Vec<Option<usize>> = scan
        .names
        .iter()
        .map(|name| sniffed.names.iter().position(|n| n.eq_ignore_ascii_case(name)))
        .collect();
    // `SchemasMatch` of the minimal sniffer: a file of one row whose types match takes the names of the scan,
    // and keeps the header option of the first file
    let single_row_match =
        !by_name.iter().all(Option::is_some) && sniffed.lines_sniffed <= 1 && same_shape && sniffed.types == scan.types;
    if single_row_match {
        return Ok(CsvFile {
            path: file.path.clone(),
            columns: sniffed.names.len(),
            header: first.header,
            skip: sniffed.skip,
            rows_until_header: sniffed.rows_until_header,
            dialect: sniffed.dialect,
            mapping: (0..scan.names.len()).map(Some).collect(),
        });
    }
    let mapping = if by_name.iter().all(Option::is_some) {
        by_name
    } else if same_shape && (!sniffed.header || !first.header) {
        (0..scan.names.len()).map(Some).collect()
    } else if scan.filename || !scan.hive.is_empty() {
        // `MultiFileOptions::AnySet`: the sniffer leaves the mismatch to `MultiFileColumnMapper`
        let missing = scan.names.iter().zip(&by_name).find(|(_, found)| found.is_none());
        let missing = missing.map_or("", |(name, _)| name.as_str());
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Failed to read file \"{path}\": schema mismatch in glob: column \"{missing}\" was read from the \
                 original file \"{original}\", but could not be found in file \"{path}\".\nCandidate names: \
                 {candidates}\nIf you are trying to read files with different schemas, try setting union_by_name=True",
                path = file.path,
                original = first.path,
                candidates = sniffed.names.join(", "),
            ),
        ));
    } else {
        let mut error = format!(
            "Schema mismatch between globbed files.\nMain file schema: {}\nCurrent file: {}\n",
            first.path, file.path
        );
        for (name, found) in scan.names.iter().zip(&by_name) {
            if found.is_none() {
                error += &format!("Column with name: \"{name}\" is missing\n");
            }
        }
        error += "Potential Fixes \n* Consider setting union_by_name=true.\n* Consider setting files_to_sniff to a \
                  higher value (e.g., files_to_sniff = -1)\n";
        return Err(Error::new(ErrorKind::InvalidInput, error));
    };
    Ok(CsvFile {
        path: file.path.clone(),
        columns: sniffed.names.len(),
        header: sniffed.header,
        skip: sniffed.skip,
        rows_until_header: sniffed.rows_until_header,
        dialect: sniffed.dialect,
        mapping,
    })
}

/// The next rows of the scan, typed. `None` when all files are done.
pub fn scan_chunk(scan: &CsvScanData, state: &mut CsvScanState) -> Result<Option<DataChunk>> {
    let Some(chunk) = scan_file_chunk(scan, state)? else {
        return Ok(None);
    };
    let path = &scan.files[state.file].path;
    crate::execution::multi_file::append_virtual_columns(
        chunk,
        &scan.virtual_columns,
        path,
        state.file,
        &mut state.virtual_rows,
    )
    .map(Some)
}

fn scan_file_chunk(scan: &CsvScanData, state: &mut CsvScanState) -> Result<Option<DataChunk>> {
    loop {
        let Some(file) = scan.files.get(state.file) else {
            flush_rejects(scan, state)?;
            return Ok(None);
        };
        if state.data.is_none() {
            let cached = if state.file == 0 {
                scan.first_data.lock().expect("not poisoned").take()
            } else {
                None
            };
            let data = match cached {
                Some(data) => data,
                None => read_csv_file(&file.path, &scan.options)?,
            };
            let bom = if data.starts_with(&[0xef, 0xbb, 0xbf]) { 3 } else { 0 };
            state.current = if state.file > 0 && scan.options.auto_detect && !scan.union_by_name {
                Some(sniff_next_file(scan, file, &data)?)
            } else {
                None
            };
            let file = state.current.as_ref().unwrap_or(file);
            let machine = Machine::new(file.dialect.clone(), bom);
            let rows = usize::from(file.header) + file.skip;
            let begin = skip_rows(&data, &machine.dialect, &machine.transitions(), bom, rows);
            state.line = count_lines(&data[..begin]);
            let mut machine = machine;
            machine.position = begin;
            state.scanner = Some(ValueScanner::new(machine));
            state.data = Some(data);
        }
        let data = state.data.take().expect("just set");
        let current = state.current.take();
        let result = read_rows(&data, scan, current.as_ref().unwrap_or(file), state)?;
        match result {
            Some(chunk) => {
                state.data = Some(data);
                state.current = current;
                return Ok(Some(chunk));
            }
            None => state.file += 1,
        }
    }
}

/// `CSVReaderOptions::ToString`
fn options_text(scan: &CsvScanData, file: &CsvFile) -> String {
    let options = &scan.options;
    let origin = |user: bool| if user { "(Set By User)" } else { "(Auto-Detected)" };
    let byte = |b: Option<u8>| b.map_or("(empty)".to_string(), |b| char::from(b).to_string());
    let dialect = &file.dialect;
    let format = |f: &Option<String>| f.clone().unwrap_or_default();
    format!(
        "  file = {}\n  delimiter = {} {}\n  quote = {} {}\n  escape = {} {}\n  new_line = {} {}\n  header = {} {}\n  \
         skip_rows = {} {}\n  comment = {} {}\n  strict_mode = {} {}\n  date_format = {} {}\n  timestamp_format = {} \
         {}\n  null_padding = {}\n  sample_size = {}\n  ignore_errors = {}\n  all_varchar = {}\n{}",
        file.path,
        String::from_utf8_lossy(&dialect.delimiter),
        origin(options.delimiter.is_some()),
        byte(dialect.quote),
        origin(options.quote.is_some()),
        byte(dialect.escape),
        origin(options.escape.is_some()),
        dialect.new_line.shown(),
        origin(options.new_line.is_some()),
        file.header,
        origin(options.header.is_some()),
        file.skip,
        origin(options.skip.is_some()),
        byte(dialect.comment),
        origin(options.comment.is_some()),
        dialect.strict,
        origin(!options.strict_mode),
        format(&scan.parsing.date_format),
        origin(options.date_format.is_some()),
        format(&scan.parsing.timestamp_format),
        origin(options.timestamp_format.is_some()),
        u8::from(options.null_padding),
        options.sample_chunks().saturating_mul(2048),
        options.ignore_errors,
        u8::from(options.all_varchar),
        scan.mismatch,
    )
}

/// `CSVErrorHandler::ThrowError`: what happened, where, and what the reader was doing.
fn csv_error(
    kind: ErrorKind,
    scan: &CsvScanData,
    file: &CsvFile,
    line: usize,
    row: &str,
    message: &str,
    fixes: &str,
) -> Error {
    let original = if row.is_empty() {
        String::new()
    } else {
        format!("Original Line: {row}\n")
    };
    Error::new(
        kind,
        format!(
            "CSV Error on Line: {line}\n{original}{message}\n{fixes}\n{}\n",
            options_text(scan, file)
        ),
    )
}

/// The fixes C++ suggests for rows it can't parse.
fn strict_fix(scan: &CsvScanData) -> &'static str {
    if scan.options.strict_mode {
        "* Disable the parser's strict mode (strict_mode=false) to allow reading rows that do not comply with the CSV \
         standard.\n"
    } else {
        ""
    }
}

/// Where a row came from.
struct RowSpan {
    line: usize,
    start: usize,
    end: usize,
}

impl RowSpan {
    /// `FullLinePosition::ReconstructCurrentLine`, made valid UTF-8 with `?`.
    fn text(&self, data: &[u8]) -> String {
        make_valid(&data[self.start..self.end], '?')
    }
}

fn reject(scan: &CsvScanData, state: &mut CsvScanState, reject: Reject) {
    if scan.rejects.is_some() {
        state.rejects.push(reject);
    }
}

/// The scan of one row that didn't parse: a line of its own in the rejects, or the error.
#[allow(clippy::too_many_arguments)] // each says something else of the row
fn bad_line(
    data: &[u8],
    scan: &CsvScanData,
    file: &CsvFile,
    state: &mut CsvScanState,
    before: usize,
    kind: &'static str,
    message: String,
    fixes: String,
) -> Result<()> {
    let line = state.line + 1;
    let mut end = before;
    while end < data.len() && data[end] != b'\n' && data[end] != b'\r' {
        end += 1;
    }
    if !scan.options.ignore_errors {
        let row = String::from_utf8_lossy(&data[before..end]).into_owned();
        return Err(csv_error(
            ErrorKind::InvalidInput,
            scan,
            file,
            line,
            &row,
            &message,
            &fixes,
        ));
    }
    let span = RowSpan {
        line,
        start: before,
        end,
    };
    let text = span.text(data);
    let scanner = state.scanner.as_mut().expect("a scan has a scanner");
    scanner.skip_line(data);
    let position = scanner.machine.position;
    state.line += count_lines(&data[before..position]);
    reject(
        scan,
        state,
        Reject {
            file: state.file,
            line,
            row_position: before + 1,
            position: Some(before + 1),
            column: Some(1),
            column_name: scan.names.first().cloned(),
            kind,
            text,
            message,
        },
    );
    Ok(())
}

fn read_rows(data: &[u8], scan: &CsvScanData, file: &CsvFile, state: &mut CsvScanState) -> Result<Option<DataChunk>> {
    let ignore = scan.options.ignore_errors;
    let padding = scan.options.null_padding;
    let strict = file.dialect.strict;
    let empty_is_null = scan.options.null_strs.iter().any(String::is_empty);
    loop {
        let mut rows: Vec<Vec<Field<'_>>> = Vec::with_capacity(CHUNK_ROWS);
        let mut spans = Vec::with_capacity(CHUNK_ROWS);
        let mut doomed = Vec::with_capacity(CHUNK_ROWS);
        while rows.len() < CHUNK_ROWS {
            let scanner = state.scanner.as_mut().expect("a scan has a scanner");
            let before = scanner.machine.position;
            let row = match scanner.next(data) {
                None => break,
                Some(Scanned::Row(row)) => row,
                Some(Scanned::EmptyLine) => {
                    let position = scanner.machine.position;
                    state.line += count_lines(&data[before..position]);
                    if file.columns == 1 && empty_is_null {
                        let field = Field {
                            text: std::borrow::Cow::Borrowed(&[]),
                            quoted: false,
                            escaped: false,
                            offset: before,
                            end: before,
                        };
                        spans.push(RowSpan {
                            line: state.line,
                            start: before,
                            end: before,
                        });
                        rows.push(vec![field]);
                        doomed.push(false);
                    }
                    continue;
                }
                Some(Scanned::Unterminated) => {
                    let fixes = format!(
                        "Possible fixes:\n{}* Enable ignore errors (ignore_errors=true) to skip this row\n* Set quote \
                         to empty or to a different value (e.g., quote='')\n",
                        strict_fix(scan)
                    );
                    bad_line(
                        data,
                        scan,
                        file,
                        state,
                        before,
                        "UNQUOTED VALUE",
                        "Value with unterminated quote found.".to_string(),
                        fixes,
                    )?;
                    continue;
                }
                Some(Scanned::Invalid) => {
                    let fixes = if scan.options.strict_mode {
                        format!("Possible fixes:\n{}", strict_fix(scan))
                    } else {
                        String::new()
                    };
                    bad_line(
                        data,
                        scan,
                        file,
                        state,
                        before,
                        "INVALID STATE",
                        "The CSV Parser state machine reached an invalid state.\nThis can happen when is not possible \
                         to parse your CSV File with the given options, or the CSV File is not RFC 4180 compliant "
                            .to_string(),
                        fixes,
                    )?;
                    continue;
                }
            };
            let position = scanner.machine.position;
            let mut row_fields = row.fields;
            let span = RowSpan {
                line: state.line + count_lines(&data[before..row.start.max(before)]) + 1,
                start: row.start,
                end: row.end,
            };
            state.line += count_lines(&data[before..position]);
            let line_size = position.saturating_sub(span.start);
            if line_size > scan.options.maximum_line_size() {
                // `CSVError::LineSizeError` of the scanner: the size of the line without its new line
                let text = span.text(data);
                if !ignore {
                    return Err(line_size_error(&scan.options, text.len()));
                }
                let message = format!(
                    "Maximum line size of {} bytes exceeded. Actual Size:{} bytes.",
                    scan.options.maximum_line_size(),
                    text.len()
                );
                drop(row_fields);
                reject(
                    scan,
                    state,
                    Reject {
                        file: state.file,
                        line: span.line,
                        row_position: span.start + 1,
                        position: Some(span.start + 1),
                        column: None,
                        column_name: None,
                        kind: "LINE SIZE OVER MAXIMUM",
                        text,
                        message,
                    },
                );
                continue;
            }
            if let Some(field) = row_fields.iter().position(|f| std::str::from_utf8(&f.text).is_err()) {
                let message = "Invalid unicode (byte sequence mismatch) detected. This file is not utf-8 encoded.";
                if !ignore {
                    return Err(csv_error(
                        ErrorKind::InvalidInput,
                        scan,
                        file,
                        span.line,
                        &span.text(data),
                        message,
                        "Possible Solution: Set the correct encoding, if available, to read this CSV File (e.g., \
                         encoding='UTF-16')\nPossible Solution: Enable ignore errors (ignore_errors=true) to skip \
                         this row\n",
                    ));
                }
                let (position, text) = (row_fields[field].offset + 1, span.text(data));
                reject(
                    scan,
                    state,
                    Reject {
                        file: state.file,
                        line: span.line,
                        row_position: span.start + 1,
                        position: Some(position),
                        column: Some(field + 1),
                        column_name: scan.names.get(field).cloned(),
                        kind: "INVALID ENCODING",
                        text,
                        message: message.to_string(),
                    },
                );
                continue;
            }
            // a NULL as the first value too many is no value (`HandleTooManyColumnsError`)
            let trailing = row_fields.len() == file.columns + 1
                && row_fields
                    .last()
                    .is_some_and(|f| is_null(f, &scan.options, &file.dialect));
            if trailing || (!strict && row_fields.len() > file.columns) {
                row_fields.truncate(file.columns);
            }
            let found = row_fields.len();
            if found != file.columns && !(padding && found < file.columns) {
                let message = format!("Expected Number of Columns: {} Found: {found}", file.columns);
                if !ignore {
                    let padding_fix = if padding {
                        ""
                    } else {
                        "* Enable null padding (null_padding=true) to replace missing values with NULL\n"
                    };
                    let fixes = format!(
                        "Possible fixes:\n{}{padding_fix}* Enable ignore errors (ignore_errors=true) to skip this \
                         row\n",
                        strict_fix(scan)
                    );
                    return Err(csv_error(
                        ErrorKind::InvalidInput,
                        scan,
                        file,
                        span.line,
                        &span.text(data),
                        &message,
                        &fixes,
                    ));
                }
                // a report per column, and what the row has is still checked below
                let text = span.text(data);
                #[allow(clippy::needless_range_loop)] // the row is indexed only past the columns
                for column in found.min(file.columns)..found.max(file.columns) {
                    let missing = found < file.columns;
                    let (kind, column_name, position) = if missing {
                        ("MISSING COLUMNS", scan.names.get(column).cloned(), span.end)
                    } else {
                        ("TOO MANY COLUMNS", None, row_fields[column].offset.saturating_sub(1))
                    };
                    let shown = if missing { column } else { column + 1 };
                    reject(
                        scan,
                        state,
                        Reject {
                            file: state.file,
                            line: span.line,
                            row_position: span.start + 1,
                            position: Some(position + 1),
                            column: Some(shown),
                            column_name,
                            kind,
                            text: text.clone(),
                            message: format!("Expected Number of Columns: {} Found: {shown}", file.columns),
                        },
                    );
                }
                rows.push(row_fields);
                spans.push(span);
                doomed.push(true);
                continue;
            }
            rows.push(row_fields);
            spans.push(span);
            doomed.push(false);
        }
        if rows.is_empty() {
            return Ok(None);
        }
        loop {
            match typed_chunk(data, &rows, scan, file, &spans, &doomed, state)? {
                Typed::Chunk(chunk) => return Ok(Some(chunk)),
                Typed::Reject(bad) => {
                    for &row in bad.iter().rev() {
                        rows.remove(row);
                        spans.remove(row);
                        doomed.remove(row);
                    }
                    if rows.is_empty() {
                        break;
                    }
                }
            }
        }
    }
}

/// What a value's text is once a file's way of writing numbers and dates is taken out. A DATE or
/// TIMESTAMP with a format of its own is the time the format parses.
fn convert_column(strings: &Vector, count: usize, ty: &LogicalType, parsing: &Parsing) -> Result<Option<Vector>> {
    let format = match ty.id() {
        LogicalTypeId::Date => parsing.date_format.as_deref(),
        LogicalTypeId::Timestamp => parsing.timestamp_format.as_deref(),
        _ => None,
    };
    let numeric = ty.id() == LogicalTypeId::Bignum
        || (ty.is_numeric() && (parsing.thousands.is_some() || parsing.decimal_separator != b'.'));
    if format.is_none() && !numeric {
        return Ok(None);
    }
    let mut result = Vector::new(ty.clone(), count);
    for row in 0..count {
        let value = strings.get_value(row);
        if value.is_null() {
            result.set_value(row, &Value::null(ty.clone()))?;
            continue;
        }
        let text = value.try_to_string()?;
        let converted = if let Some(format) = format {
            let micros = strptime_micros(&text, format).map_err(|e| Error::new(ErrorKind::Conversion, e))?;
            let timestamp = Timestamp(micros);
            if ty.id() == LogicalTypeId::Date {
                Value::date(timestamp.date())
            } else {
                Value::timestamp(timestamp)
            }
        } else {
            let Some(clean) = numeric_text(&text, ty, parsing) else {
                return Err(Error::new(ErrorKind::Conversion, "not a number"));
            };
            let mut single = Vector::new(LogicalType::VARCHAR, 1);
            single.set_value(0, &Value::varchar(clean))?;
            let mut typed = Vector::new(ty.clone(), 1);
            cast_vector(&single, &mut typed, 1, CastMode::Strict)?;
            typed.get_value(0)
        };
        result.set_value(row, &converted)?;
    }
    Ok(Some(result))
}

/// Whether a row's value is one a column of `ty` can hold.
fn value_fits(strings: &Vector, row: usize, ty: &LogicalType, parsing: &Parsing) -> bool {
    let value = strings.get_value(row);
    if value.is_null() {
        return true;
    }
    if parsing.date_format.is_some() || parsing.timestamp_format.is_some() || ty.is_numeric() {
        let special = matches!(ty.id(), LogicalTypeId::Date | LogicalTypeId::Timestamp);
        if (special && (parsing.date_format.is_some() || parsing.timestamp_format.is_some()))
            || parsing.thousands.is_some()
            || parsing.decimal_separator != b'.'
        {
            return value.try_to_string().is_ok_and(|text| {
                if special {
                    return can_cast(&text, ty, parsing);
                }
                let Some(clean) = numeric_text(&text, ty, parsing) else {
                    return false;
                };
                let mut single = Vector::new(LogicalType::VARCHAR, 1);
                single.set_value(0, &Value::varchar(clean)).is_ok()
                    && cast_vector(&single, &mut Vector::new(ty.clone(), 1), 1, CastMode::Strict).is_ok()
            });
        }
    }
    converts(strings, row, ty)
}

/// Whether the value of a row can be cast to `ty`.
fn converts(strings: &Vector, row: usize, ty: &LogicalType) -> bool {
    let mut single = Vector::new(LogicalType::VARCHAR, 1);
    if single.set_value(0, &strings.get_value(row)).is_err() {
        return false;
    }
    cast_vector(&single, &mut Vector::new(ty.clone(), 1), 1, CastMode::Strict).is_ok()
}

enum Typed {
    Chunk(DataChunk),
    /// the rows to drop, ascending
    Reject(Vec<usize>),
}

fn typed_chunk(
    data: &[u8],
    rows: &[Vec<Field<'_>>],
    scan: &CsvScanData,
    file: &CsvFile,
    spans: &[RowSpan],
    doomed: &[bool],
    state: &mut CsvScanState,
) -> Result<Typed> {
    let count = rows.len();
    let mut result = Vec::with_capacity(scan.types.len() + 1);
    let mut bad: std::collections::BTreeSet<usize> = (0..count).filter(|&row| doomed[row]).collect();
    for (index, (ty, column)) in scan.types.iter().zip(&file.mapping).enumerate() {
        let fields: Vec<_> = rows.iter().map(|row| column.and_then(|c| row.get(c))).collect();
        let mut strings = varchar_vector(&fields, &scan.options, &file.dialect)?;
        if scan.force_not_null.get(index).copied().unwrap_or(false) {
            for row in 0..count {
                if column.is_some() && strings.get_value(row).is_null() {
                    strings.set_value(row, &Value::varchar(""))?;
                }
            }
        }
        if *ty == LogicalType::VARCHAR {
            result.push(strings);
            continue;
        }
        let mut typed = Vector::new(ty.clone(), count);
        // a failed cast says nothing of which rows failed, so only then each is tried alone
        let converted = match convert_column(&strings, count, ty, &scan.parsing) {
            Ok(Some(column)) => {
                typed = column;
                true
            }
            Ok(None) => cast_vector(&strings, &mut typed, count, CastMode::Strict).is_ok(),
            Err(_) => false,
        };
        let failed: Vec<usize> = if converted {
            Vec::new()
        } else {
            (0..count)
                .filter(|&row| !value_fits(&strings, row, ty, &scan.parsing))
                .collect()
        };
        for row in failed {
            let message = format!(
                "Error when converting column \"{}\". Could not convert string \"{}\" to '{ty}'",
                scan.names[index],
                strings.get_value(row).try_to_string()?
            );
            if !scan.options.ignore_errors {
                let name = &scan.names[index];
                let mut fixes = format!(
                    "Column {name} is being converted as type {}\n",
                    ty.to_string().split('(').next().unwrap_or_default()
                );
                if scan.manual_types.get(index).copied().unwrap_or(false) {
                    fixes += "This type was either manually set or derived from an existing table. Select a \
                              different type to correctly parse this column.\n";
                } else {
                    fixes += &format!(
                        "This type was auto-detected from the CSV file.\nPossible solutions:\n* Override the type \
                         for this column manually by setting the type explicitly, e.g., types={{'{name}': \
                         'VARCHAR'}}\n* Set the sample size to a larger value to enable the auto-detection to scan \
                         more values, e.g., sample_size=-1\n* Use a COPY statement to automatically derive types \
                         from an existing table.\n"
                    );
                }
                fixes += "* Check whether the null string value is set correctly (e.g., nullstr = 'N/A')\n";
                return Err(csv_error(
                    ErrorKind::Conversion,
                    scan,
                    file,
                    spans[row].line,
                    &spans[row].text(data),
                    &message,
                    &fixes,
                ));
            }
            bad.insert(row);
            reject(
                scan,
                state,
                Reject {
                    file: state.file,
                    line: spans[row].line,
                    row_position: spans[row].start + 1,
                    position: fields[row].map(|field| field.offset + 1),
                    column: Some(index + 1),
                    column_name: Some(scan.names[index].clone()),
                    kind: "CAST",
                    text: spans[row].text(data),
                    // the rows the scan goes on after name the type by its id (`LogicalTypeIdToString`)
                    message: format!(
                        "Error when converting column \"{}\". Could not convert string \"{}\" to '{}'",
                        scan.names[index],
                        strings.get_value(row).try_to_string()?,
                        ty.to_string().split('(').next().unwrap_or_default()
                    ),
                },
            );
        }
        result.push(typed);
    }
    if !bad.is_empty() {
        return Ok(Typed::Reject(bad.into_iter().collect()));
    }
    if scan.filename {
        let mut names = Vector::new(LogicalType::VARCHAR, count);
        let name = Value::varchar(file.path.as_str());
        for i in 0..count {
            names.set_value(i, &name)?;
        }
        result.push(names);
    }
    if !scan.hive.is_empty() {
        for (key, ty) in &scan.hive {
            let value = hive_value(&file.path, key, ty)?;
            let mut typed = Vector::new(ty.clone(), count);
            for i in 0..count {
                typed.set_value(i, &value)?;
            }
            result.push(typed);
        }
    }
    Ok(Typed::Chunk(DataChunk::from_columns(result, count)))
}

/// Appends rows of text to a table, cast to its columns.
fn append_text(table: &TableCatalogEntry, rows: &[Vec<Option<String>>]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut columns = Vec::new();
    for (column, ty) in table.types().iter().enumerate() {
        let mut strings = Vector::new(LogicalType::VARCHAR, rows.len());
        for (i, row) in rows.iter().enumerate() {
            let value = row[column].as_ref().map_or_else(
                || Value::null(LogicalType::VARCHAR),
                |text| Value::varchar(text.as_str()),
            );
            strings.set_value(i, &value)?;
        }
        let mut typed = Vector::new(ty.clone(), rows.len());
        cast_vector(&strings, &mut typed, rows.len(), CastMode::Strict)?;
        columns.push(typed);
    }
    table
        .append_storage()?
        .append(&DataChunk::from_columns(columns, rows.len()));
    Ok(())
}

/// The start of `text`, at most `limit` bytes that stay valid UTF-8.
/// `rejects_line_size_limit`: the first `limit` bytes, the bytes of a character it cuts made `.`
/// (`Utf8Proc::MakeValid` with `'.'`).
fn cut(text: &str, limit: usize) -> String {
    if limit == 0 || text.len() <= limit {
        return text.to_string();
    }
    let end = (0..=limit).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let mut result = text[..end].to_string();
    result.extend(std::iter::repeat_n('.', limit - end));
    result
}

/// `Utf8Proc::MakeValid`: the bytes of each invalid sequence replaced with `flag`.
pub(crate) fn make_valid(bytes: &[u8], flag: char) -> String {
    let mut result = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                result.push_str(valid);
                return result;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                result.push_str(std::str::from_utf8(valid).expect("checked"));
                let invalid = error.error_len().unwrap_or(after.len());
                result.extend(std::iter::repeat_n(flag, invalid));
                rest = &after[invalid..];
            }
        }
    }
}

/// `Utf8Proc::RemoveInvalid`: the bytes of each invalid sequence left out.
pub(crate) fn remove_invalid(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                result.push_str(valid);
                return result;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                result.push_str(std::str::from_utf8(valid).expect("checked"));
                rest = &after[error.error_len().unwrap_or(after.len())..];
            }
        }
    }
}

/// `CSVGlobalState::FillRejectsTable`.
fn flush_rejects(scan: &CsvScanData, state: &mut CsvScanState) -> Result<()> {
    let Some(rejects) = &scan.rejects else {
        return Ok(());
    };
    if state.flushed {
        return Ok(());
    }
    state.flushed = true;
    let mut found = std::mem::take(&mut state.rejects);
    if rejects.limit != 0 {
        found.truncate(rejects.limit);
    }
    let text = |n: usize| Some(n.to_string());
    let errors: Vec<Vec<Option<String>>> = found
        .iter()
        .map(|r| {
            vec![
                Some(rejects.scan_id.to_string()),
                text(r.file),
                text(r.line),
                text(r.row_position),
                r.position.map(|p| p.to_string()),
                r.column.map(|c| c.to_string()),
                r.column_name.clone(),
                Some(r.kind.to_string()),
                Some(cut(&r.text, rejects.line_size_limit)),
                Some(r.message.clone()),
            ]
        })
        .collect();
    let shown = |byte: Option<u8>| Some(byte.map_or("(empty)".to_string(), |b| char::from(b).to_string()));
    let mut scans = Vec::new();
    for (index, file) in scan.files.iter().enumerate() {
        if !found.iter().any(|r| r.file == index) {
            continue;
        }
        let columns: Vec<String> = scan
            .names
            .iter()
            .zip(&scan.types)
            .map(|(name, ty)| format!("'{}': '{ty}'", name.replace('\'', "''")))
            .collect();
        let new_line = new_line_identifier(&file.dialect);
        scans.push(vec![
            Some(rejects.scan_id.to_string()),
            text(index),
            Some(file.path.clone()),
            Some(String::from_utf8_lossy(&file.dialect.delimiter).into_owned()),
            shown(file.dialect.quote),
            shown(file.dialect.escape),
            Some(new_line.to_string()),
            text(file.skip),
            Some(file.header.to_string()),
            Some(format!("{{{}}}", columns.join(","))),
            None,
            None,
            rejects.user_arguments.clone(),
        ]);
    }
    append_text(&rejects.errors, &errors)?;
    append_text(&rejects.scans, &scans)
}

/// What `COPY ... TO` was told (`CSVWriterOptions`).
#[derive(Debug, Clone)]
pub struct CsvWriteOptions {
    pub delimiter: Vec<u8>,
    pub quote: Option<u8>,
    pub escape: Option<u8>,
    pub comment: Option<u8>,
    pub header: bool,
    pub date_format: Option<String>,
    pub timestamp_format: Option<String>,
    /// rows go out as JSON objects of these columns, a `new_line` between them
    pub json_names: Option<Vec<String>>,
    pub json_rows: Arc<std::sync::atomic::AtomicU64>,
    pub null_str: String,
    pub new_line: String,
    pub prefix: String,
    pub suffix: String,
    /// the columns that are always quoted, by position
    pub force_quote: Vec<bool>,
    /// `PARTITION_BY`: a directory of files, one per partition
    pub partitioning: Option<super::parquet::Partitioning>,
    /// `FILE_SIZE_BYTES` and `PER_THREAD_OUTPUT`: a directory of files
    pub rotation: Option<super::parquet::Rotation>,
    pub output: super::parquet::CopyOutputOptions,
    /// `FORMAT BLOB`: the bytes of the values, nothing else
    pub raw_blob: bool,
}

impl Default for CsvWriteOptions {
    fn default() -> Self {
        Self {
            delimiter: b",".to_vec(),
            quote: Some(b'"'),
            escape: Some(b'"'),
            comment: Some(b'#'),
            date_format: None,
            timestamp_format: None,
            json_names: None,
            json_rows: Arc::default(),
            header: true,
            null_str: String::new(),
            new_line: "\n".to_string(),
            prefix: String::new(),
            suffix: String::new(),
            force_quote: Vec::new(),
            raw_blob: false,
            partitioning: None,
            rotation: None,
            output: super::parquet::CopyOutputOptions::default(),
        }
    }
}

impl CsvWriteOptions {
    /// `CSVWriter::RequiresQuotes`.
    fn requires_quotes(&self, text: &[u8]) -> bool {
        text == self.null_str.as_bytes()
            || text.iter().any(|&c| {
                c == b'\n'
                    || c == b'\r'
                    || Some(&c) == self.delimiter.first()
                    || Some(c) == self.quote
                    || Some(c) == self.comment
            })
    }

    /// `CSVWriter::WriteQuotedString`.
    pub fn write_value(&self, out: &mut Vec<u8>, text: &[u8], force: bool) {
        let Some(quote) = self.quote.filter(|_| force || self.requires_quotes(text)) else {
            out.extend_from_slice(text);
            return;
        };
        let escape = self.escape.unwrap_or(quote);
        out.push(quote);
        for &c in text {
            if c == quote || (c == escape && escape != quote) {
                out.push(escape);
            }
            out.push(c);
        }
        out.push(quote);
    }

    pub fn write_header(&self, out: &mut Vec<u8>, names: &[String]) {
        let columns: Vec<usize> = (0..names.len()).collect();
        self.write_header_of(out, names, &columns);
    }

    /// The header of a file that holds `columns` of the rows.
    pub fn write_header_of(&self, out: &mut Vec<u8>, names: &[String], columns: &[usize]) {
        if !self.prefix.is_empty() {
            out.extend_from_slice(self.prefix.as_bytes());
        }
        if !self.header {
            return;
        }
        for (i, &column) in columns.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(&self.delimiter);
            }
            let force = self.force_quote.get(column).copied().unwrap_or(false);
            self.write_value(out, names[column].as_bytes(), force);
        }
        out.extend_from_slice(self.new_line.as_bytes());
    }

    /// A value as text: with the format of its type, if there is one.
    fn formatted(&self, value: &Value) -> Result<String> {
        match self.formatted_temporal(value)? {
            Some(text) => Ok(text),
            None => value.try_to_string(),
        }
    }

    /// A date or timestamp in the format the COPY gave for its type, if it gave one.
    fn formatted_temporal(&self, value: &Value) -> Result<Option<String>> {
        const MICROS_PER_DAY: i64 = 86_400_000_000;
        match (value.logical_type().id(), value.data()) {
            (LogicalTypeId::Date, ValueData::Int32(days)) if self.date_format.is_some() => {
                strftime_text(self.date_format.as_deref().unwrap_or_default(), *days, 0, false).map(Some)
            }
            (LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz, ValueData::Int64(micros))
                if self.timestamp_format.is_some() =>
            {
                strftime_text(
                    self.timestamp_format.as_deref().unwrap_or_default(),
                    micros.div_euclid(MICROS_PER_DAY) as i32,
                    micros.rem_euclid(MICROS_PER_DAY),
                    value.logical_type().id() == LogicalTypeId::TimestampTz,
                )
                .map(Some)
            }
            _ => Ok(None),
        }
    }

    pub fn write_chunk(&self, out: &mut Vec<u8>, chunk: &DataChunk) -> Result<()> {
        let rows: Vec<usize> = (0..chunk.len()).collect();
        let columns: Vec<usize> = (0..chunk.column_count()).collect();
        self.write_rows(out, chunk, &rows, &columns, &self.json_rows)
    }

    /// Writes `columns` of `rows` of `chunk`; `json_rows` counts the JSON objects of the file so far.
    pub fn write_rows(
        &self,
        out: &mut Vec<u8>,
        chunk: &DataChunk,
        rows: &[usize],
        columns: &[usize],
        json_rows: &std::sync::atomic::AtomicU64,
    ) -> Result<()> {
        if self.raw_blob {
            for &row in rows {
                if let ValueData::Bytes(bytes) = chunk.get_value(columns[0], row).data() {
                    out.extend_from_slice(bytes);
                }
            }
            return Ok(());
        }
        if let Some(names) = &self.json_names {
            for &row in rows {
                let mut items = Vec::with_capacity(columns.len());
                for &column in columns {
                    let value = chunk.get_value(column, row);
                    let value = match self.formatted_temporal(&value)? {
                        Some(text) => Value::varchar(text),
                        None => value,
                    };
                    items.push((names[column].clone(), value_to_json(&value)?));
                }
                if json_rows.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0 {
                    out.extend_from_slice(self.new_line.as_bytes());
                }
                out.extend_from_slice(Json::Object(items).text().as_bytes());
            }
            return Ok(());
        }
        for &row in rows {
            for (i, &column) in columns.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(&self.delimiter);
                }
                let value = chunk.get_value(column, row);
                if value.is_null() {
                    out.extend_from_slice(self.null_str.as_bytes());
                } else {
                    let force = self.force_quote.get(column).copied().unwrap_or(false);
                    let text = self.formatted(&value)?;
                    self.write_value(out, text.as_bytes(), force);
                }
            }
            out.extend_from_slice(self.new_line.as_bytes());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::csv_sniffer::sniff;
    use super::*;

    fn rows(data: &[u8], dialect: Dialect) -> Vec<Vec<String>> {
        let mut scanner = ValueScanner::new(Machine::new(dialect, 0));
        let mut result = Vec::new();
        while let Some(Scanned::Row(row)) = scanner.next(data) {
            result.push(
                row.fields
                    .iter()
                    .map(|f| String::from_utf8_lossy(&f.text).into_owned())
                    .collect(),
            );
        }
        result
    }

    fn dialect(delimiter: &[u8]) -> Dialect {
        Dialect {
            delimiter: delimiter.to_vec(),
            quote: Some(b'"'),
            escape: Some(b'"'),
            comment: None,
            new_line: NewLine::SingleN,
            strict: true,
        }
    }

    #[test]
    fn rows_with_quotes_and_newlines() {
        let data = b"a,\"b\nc\",\"d\"\"e\"\n1,2,3\n";
        assert_eq!(
            rows(data, dialect(b",")),
            vec![vec!["a", "b\nc", "d\"e"], vec!["1", "2", "3"]]
        );
    }

    #[test]
    fn multi_byte_delimiter() {
        assert_eq!(
            rows(b"a||b\nc||\n", dialect(b"||")),
            vec![vec!["a", "b"], vec!["c", ""]]
        );
    }

    #[test]
    fn unterminated_quote() {
        let mut scanner = ValueScanner::new(Machine::new(dialect(b","), 0));
        assert!(matches!(scanner.next(b"\"abc"), Some(Scanned::Unterminated)));
    }

    #[test]
    fn sniffs_dialect_header_and_types() {
        let data = b"name;age\nx;1\ny;2\n";
        let sniffed = sniff(data, "x.csv", &CsvReadOptions::default(), None).unwrap();
        assert_eq!(sniffed.dialect.delimiter, b";");
        assert!(sniffed.header);
        assert_eq!(sniffed.names, vec!["name", "age"]);
        assert_eq!(sniffed.types, vec![LogicalType::VARCHAR, LogicalType::BIGINT]);
    }

    #[test]
    fn no_header_generates_names() {
        let sniffed = sniff(b"1,2\n3,4\n", "x.csv", &CsvReadOptions::default(), None).unwrap();
        assert!(!sniffed.header);
        assert_eq!(sniffed.names, vec!["column0", "column1"]);
    }

    #[test]
    fn detects_date_formats() {
        let sniffed = sniff(
            b"d\n31/12/2020\n01/02/2021\n",
            "x.csv",
            &CsvReadOptions::default(),
            None,
        )
        .unwrap();
        assert_eq!(sniffed.types, vec![LogicalType::DATE]);
        assert_eq!(sniffed.date_format.as_deref(), Some("%d/%m/%Y"));
    }

    #[test]
    fn writes_quotes_where_needed() {
        let options = CsvWriteOptions::default();
        let mut out = Vec::new();
        options.write_value(&mut out, b"a,b", false);
        options.write_value(&mut out, b"plain", false);
        assert_eq!(out, b"\"a,b\"plain");
    }
}
