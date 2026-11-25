// Ported from duckdb/src/execution/operator/csv_scanner/ and duckdb/src/common/csv_writer.cpp
// @ rust-port-baseline
//
//! Reading and writing CSV. A file is scanned a row at a time through C++'s state machine (`csv_state_machine`):
//! read whole, or one larger than a buffer a buffer at a time ([`Stream`]); by one thread, or by threads at once in
//! parts that start where its rows do ([`parallel_morsels`]). The sniffer is in `csv_sniffer`.

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
    pub first_data: std::sync::Mutex<Option<Arc<Vec<u8>>>>,
    /// the columns of `output_types` the scan outputs, in order (`column_ids`); `None` for all.
    /// The others are not converted, so their values cannot fail.
    pub projection: Option<Vec<usize>>,
    /// the order the binder read the columns in, which projection pushdown keeps
    pub column_order: crate::execution::multi_file::ColumnOrder,
    /// `ExtraOperatorInfo::file_filters`: the filters that left files out, as text
    pub file_filters: String,
    /// how many files there were before filters left some out
    pub total_files: Option<usize>,
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
    /// the id of its first file: the scans of one query number their files on from each other
    pub file_offset: usize,
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

/// The lines that end in `data[start..end]`: a `\r\n` is one, even when the range ends between its two bytes (the
/// `\r` counts, and the `\n` after it does not).
fn count_lines(data: &[u8], start: usize, end: usize) -> usize {
    let mut count = 0;
    for i in start..end {
        let c = data[i];
        count += usize::from(c == b'\r' || (c == b'\n' && (i == 0 || data[i - 1] != b'\r')));
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

    /// The types of the columns the scan outputs.
    pub fn projected_types(&self) -> Vec<LogicalType> {
        let types = self.output_types();
        match &self.projection {
            Some(columns) => columns.iter().map(|&c| types[c].clone()).collect(),
            None => types,
        }
    }

    /// The output columns the scan outputs, by their position among all of them.
    pub fn projected_columns(&self) -> Vec<usize> {
        match &self.projection {
            Some(columns) => columns.clone(),
            None => (0..self.output_types().len()).collect(),
        }
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
        match field {
            Some(field) if !is_null(field, options, dialect) => {
                // most fields are ASCII, which is quicker to tell than UTF-8 in general
                if !field.text.is_ascii() && std::str::from_utf8(&field.text).is_err() {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Invalid unicode (byte sequence mismatch) detected in CSV file",
                    ));
                }
                result.set_str(i, &field.text);
            }
            _ => result.set_null(i, true),
        }
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
    data: Option<Arc<Vec<u8>>>,
    scanner: Option<ValueScanner>,
    /// the file being read, if sniffing it found other than the first
    current: Option<CsvFile>,
    line: usize,
    rejects: Vec<Reject>,
    flushed: bool,
    virtual_rows: crate::execution::multi_file::VirtualRows,
    /// of a file read a buffer at a time, where `data` is in it
    stream: Option<Stream>,
    /// every file is read whole, for the error the scan of it finds first
    whole: bool,
    /// the size of the file being read, and how much of it was counted as scanned
    file_bytes: u64,
    bytes_counted: u64,
    /// the bytes scanned not yet given to the profiler
    bytes_pending: u64,
}

/// A file read a buffer at a time (`CSVBufferManager`): the scan has the bytes of it from `base` on.
#[derive(Debug)]
struct Stream {
    path: String,
    base: u64,
    size: u64,
    buffer: usize,
    /// a row is scanned with at least this many bytes after its start, or the end of the file: twice the longest
    /// line, so that a line longer than that is one, and an error
    margin: usize,
}

impl Stream {
    fn at_end(&self, data: &[u8]) -> bool {
        self.base + data.len() as u64 >= self.size
    }
}

/// Of a stream, the bytes after where the scanner is topped up from the file, those before it dropped, once fewer
/// than its margin are left (or with `force`, if any are left to read).
fn refill(stream: &mut Stream, data: &mut Arc<Vec<u8>>, scanner: &mut ValueScanner, force: bool) -> Result<()> {
    if stream.at_end(data) || (!force && data.len() - scanner.machine.position >= stream.margin) {
        return Ok(());
    }
    let cut = scanner.earliest();
    let more = super::file_system::read_range(&stream.path, stream.base + data.len() as u64, stream.buffer)?;
    let mut next = Vec::with_capacity(data.len() - cut + more.len());
    next.extend_from_slice(&data[cut..]);
    next.extend_from_slice(&more);
    scanner.rebase(cut);
    stream.base += cut as u64;
    *data = Arc::new(next);
    Ok(())
}

/// The error a scan of all the files, each read whole, finds first: that of a scan of parts or buffers of them,
/// whose lines and positions are those of the whole file. `error` if, after all, it finds none.
fn first_error(scan: &CsvScanData, error: Error) -> Error {
    let mut whole = CsvScanState {
        whole: true,
        ..CsvScanState::default()
    };
    loop {
        match scan_chunk(scan, &mut whole) {
            Ok(Some(_)) => {}
            Ok(None) => return error,
            Err(first) => return first,
        }
    }
}

/// The bytes of its files the scan read since this was last asked (`CSVFileScan::CountBytesScanned`).
pub fn take_bytes_scanned(state: &mut CsvScanState) -> u64 {
    std::mem::take(&mut state.bytes_pending)
}

/// `CSVBufferManager`: what of the file the buffers read so far hold, the file being read in buffers of
/// `buffer_size` up to where the scanner is.
fn count_bytes(state: &mut CsvScanState, buffer_size: usize, done: bool) {
    let read = match (&state.scanner, &state.data) {
        _ if done => state.file_bytes,
        // of a file read a buffer at a time, the buffers read so far
        _ if state.stream.is_some() => {
            let stream = state.stream.as_ref().expect("just checked");
            let data = state.data.as_ref().map_or(0, |data| data.len() as u64);
            (stream.base + data).min(state.file_bytes)
        }
        (Some(scanner), Some(data)) if !data.is_empty() => {
            let at =
                (scanner.machine.position.min(data.len()) as f64 / data.len() as f64 * state.file_bytes as f64) as u64;
            let buffer = buffer_size.max(1) as u64;
            (at / buffer + 1).saturating_mul(buffer).min(state.file_bytes)
        }
        _ => state.bytes_counted,
    };
    if read > state.bytes_counted {
        state.bytes_pending += read - state.bytes_counted;
        state.bytes_counted = read;
    }
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

/// `buffer_size_option` of the options (`CSVReaderOptions::Verify`): what a buffer of a file holds, `ROWS_PER_BUFFER`
/// lines of the default longest size, or of a longer one the user set.
pub fn buffer_size(options: &CsvReadOptions) -> usize {
    const MAX_LINE_SIZE_DEFAULT: usize = 2_000_000;
    options.buffer_size.unwrap_or_else(|| {
        let line = options.max_line_size.filter(|line| *line > MAX_LINE_SIZE_DEFAULT);
        super::file_system::BUFFER_ROWS.saturating_mul(line.unwrap_or(MAX_LINE_SIZE_DEFAULT))
    })
}

/// The size of the file at `path`, if it is larger than a buffer and is read as it is: one that is read a buffer at
/// a time.
pub fn streamed_size(path: &str, options: &CsvReadOptions) -> Option<u64> {
    let encoding = options.encoding.as_deref().unwrap_or("utf-8").to_lowercase();
    if !matches!(encoding.as_str(), "utf-8" | "utf8") {
        return None;
    }
    let size = super::file_system::plain_file_size(path, options.compression)?;
    (size > buffer_size(options) as u64).then_some(size)
}

/// `CSVIterator::ROWS_PER_THREAD`: a thread scans as many bytes of a file as this many lines of the longest size
/// (`BytesPerThread`).
const ROWS_PER_THREAD: usize = 4;

/// The parts of a file that threads scan at once (`CSVBoundary`): each starts where a line does, after a `\n`.
#[derive(Debug)]
pub struct CsvMorsels {
    data: MorselData,
    starts: Vec<usize>,
    end: usize,
}

/// Where the parts of a file are: in what sniffing read of it, or in the file, which each part reads of.
#[derive(Debug)]
enum MorselData {
    Memory(Arc<Vec<u8>>),
    File(String),
}

impl CsvMorsels {
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }
}

/// The parts of the scan's file for threads, if it is one they can scan at once and gives each more than one.
///
/// C++ starts each part at what looks like the start of a line (`StringValueScanner::SetStart`), which a newline in
/// a quoted value can fool. Here a part starts after a `\n` that ends a row: in a file with no quote or escape in
/// it, any `\n`; after the first one, the state machine runs on from the last part's start to the row ends past
/// each next one ([`RowEnds`]). Each part then scans to the rows scanning the whole file does. A scan of more than
/// one file, of rejects, or of what one is ignoring, is a scan on one thread.
pub fn parallel_morsels(scan: &CsvScanData) -> Option<CsvMorsels> {
    let [file] = scan.files.as_slice() else {
        return None;
    };
    let options = &scan.options;
    if scan.filename
        || !scan.hive.is_empty()
        // the virtual columns come after the file's, and number its rows from its start
        || !(scan.virtual_columns.is_empty()
            || scan.projection.as_ref().is_some_and(|columns| columns.iter().all(|&c| c < scan.types.len())))
        || scan.rejects.is_some()
        || options.ignore_errors
        || matches!(file.dialect.new_line, NewLine::SingleR)
    {
        return None;
    }
    let bytes_per_thread = options.maximum_line_size().saturating_mul(ROWS_PER_THREAD).max(1);
    let mut first = scan.first_data.lock().expect("not poisoned");
    let in_memory = first.as_ref().map(|data| data.len() as u64);
    let size = match in_memory {
        Some(size) => size,
        None => streamed_size(&file.path, options)?,
    } as usize;
    if size < 2 * bytes_per_thread {
        return None;
    }
    // the file a buffer at a time, as the scan of it would read it: in what sniffing read, or from the file
    let buffer = buffer_size(options).max(1);
    let read = |at: usize| -> Option<Arc<Vec<u8>>> {
        match first.as_ref() {
            Some(data) => (at == 0).then(|| Arc::clone(data)),
            None => super::file_system::read_range(&file.path, at as u64, buffer)
                .ok()
                .map(Arc::new),
        }
    };
    let head = read(0)?;
    let bom = if head.starts_with(&[0xef, 0xbb, 0xbf]) { 3 } else { 0 };
    let machine = Machine::new(file.dialect.clone(), bom);
    let rows = usize::from(file.header) + file.skip;
    let begin = skip_rows(&head, &machine.dialect, &machine.transitions(), bom, rows);
    if begin + bytes_per_thread > head.len() {
        return None;
    }
    let mut starts = vec![begin];
    let mut at = begin + bytes_per_thread;
    let (mut offset, mut data) = (0, head);
    let quotes: Vec<u8> = [file.dialect.quote, file.dialect.escape]
        .into_iter()
        .flatten()
        .collect();
    // the state machine, from where it runs on
    let mut exact: Option<(Machine, usize)> = None;
    loop {
        if exact.is_none() {
            let from = if offset == 0 { begin } else { 0 };
            if quotes
                .iter()
                .any(|&byte| memchr::memchr(byte, &data[from.min(data.len())..]).is_some())
            {
                // a row starts at the last part's start: from it on, a value can hold a `\n`
                let last = *starts.last().expect("one part");
                if last < offset {
                    (offset, data) = (last, read(last)?);
                }
                let machine = Machine::with_transitions(file.dialect.clone(), machine.transitions(), 0);
                exact = Some((machine, last - offset));
            }
        }
        if let Some((machine, from)) = &mut exact {
            machine.position = std::mem::take(from);
            let mut ends = RowEnds {
                offset,
                next: at,
                size,
                bytes_per_thread,
                carry_on: file.dialect.new_line == NewLine::CarryOn,
                starts: &mut starts,
                invalid: false,
            };
            machine.process(&data, &mut ends);
            if ends.invalid {
                return None;
            }
            at = ends.next;
        } else {
            // the next start in this buffer, or past it
            while at < offset + data.len() {
                let Some(newline) = memchr::memchr(b'\n', &data[at - offset..]) else {
                    at = offset + data.len();
                    break;
                };
                let start = at + newline + 1;
                if start >= size {
                    at = size;
                    break;
                }
                starts.push(start);
                at = start + bytes_per_thread;
            }
        }
        offset += data.len();
        if offset >= size || data.is_empty() {
            break;
        }
        data = read(offset)?;
    }
    if starts.len() < 2 {
        return None;
    }
    let data = match first.take() {
        Some(data) => MorselData::Memory(data),
        None => MorselData::File(file.path.clone()),
    };
    Some(CsvMorsels {
        data,
        starts,
        end: size,
    })
}

/// Where parts start in a file the state machine runs over a buffer at a time: after the `\n` of the first row end
/// at or past where the next part can start. A row that ends with a `\r\n` across buffers is passed over.
struct RowEnds<'a> {
    /// where the buffer is in the file
    offset: usize,
    next: usize,
    size: usize,
    bytes_per_thread: usize,
    /// a `\r\n` ends a row, not a `\r` and an empty line
    carry_on: bool,
    starts: &'a mut Vec<usize>,
    invalid: bool,
}

impl RowEnds<'_> {
    /// A row ended at the byte at `position`.
    fn ended(&mut self, data: &[u8], position: usize) {
        let after = match data[position] {
            b'\n' => position + 1,
            b'\r' if self.carry_on && data.get(position + 1) == Some(&b'\n') => position + 2,
            _ => return,
        };
        let start = self.offset + after;
        if start >= self.next && start < self.size {
            self.starts.push(start);
            self.next = start + self.bytes_per_thread;
        }
    }
}

impl super::csv_state_machine::Sink for RowEnds<'_> {
    fn add_value(&mut self, _machine: &Machine, _data: &[u8], _position: usize) {}

    fn add_row(&mut self, _machine: &Machine, data: &[u8], position: usize) -> bool {
        self.ended(data, position);
        false
    }

    fn empty_line(&mut self, _machine: &Machine, _position: usize) -> bool {
        false
    }

    fn invalid_state(&mut self) {
        self.invalid = true;
    }

    fn is_comment_set(&self) -> bool {
        false
    }

    fn set_comment(&mut self, _machine: &Machine, _position: usize) {}

    fn unset_comment(&mut self, _machine: &Machine, _data: &[u8], _position: usize) -> bool {
        false
    }
}

/// The rows of part `index` of `morsels`, typed, in order. If they have an error, it is the one the scan of the
/// whole file on one thread finds first: the part has no line numbers of the file, and an earlier part may fail too.
pub fn scan_morsel(
    scan: &CsvScanData,
    morsels: &CsvMorsels,
    index: usize,
    emit: &mut dyn FnMut(DataChunk) -> Result<()>,
) -> Result<()> {
    let file = &scan.files[0];
    let start = morsels.starts[index];
    let end = morsels.starts.get(index + 1).copied().unwrap_or(morsels.end);
    let read;
    let data: &[u8] = match &morsels.data {
        MorselData::Memory(data) => &data[start..end],
        MorselData::File(path) => {
            read = super::file_system::read_range(path, start as u64, end - start)?;
            &read
        }
    };
    let mut state = CsvScanState {
        scanner: Some(ValueScanner::new(Machine::new(file.dialect.clone(), 0))),
        ..CsvScanState::default()
    };
    loop {
        match read_rows(data, scan, file, &mut state, None) {
            Ok(Some(chunk)) => emit(project(scan, chunk))?,
            Ok(None) => return Ok(()),
            // the whole file again, read from it, as the parts took what sniffing read
            Err(error) => return Err(first_error(scan, error)),
        }
    }
}

/// The bytes of the file a scan in parts reads: all of them, which the first part counts.
pub fn morsel_bytes(scan: &CsvScanData, morsels: &CsvMorsels, index: usize) -> u64 {
    if index != 0 {
        return 0;
    }
    std::fs::metadata(&scan.files[0].path).map_or(morsels.end as u64, |m| m.len())
}

/// The columns of `chunk` that the scan outputs: of a part of a file, none of them virtual.
fn project(scan: &CsvScanData, chunk: DataChunk) -> DataChunk {
    match &scan.projection {
        Some(columns) => {
            let count = chunk.len();
            DataChunk::from_columns(columns.iter().map(|&c| chunk.column(c).clone()).collect(), count)
        }
        None => chunk,
    }
}

/// The next rows of the scan, typed. `None` when all files are done.
pub fn scan_chunk(scan: &CsvScanData, state: &mut CsvScanState) -> Result<Option<DataChunk>> {
    let Some(chunk) = scan_file_chunk(scan, state)? else {
        return Ok(None);
    };
    let path = &scan.files[state.file].path;
    let chunk = crate::execution::multi_file::append_virtual_columns(
        chunk,
        &scan.virtual_columns,
        path,
        state.file,
        &mut state.virtual_rows,
    )?;
    Ok(Some(project(scan, chunk)))
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
            // a file larger than a buffer is read a buffer at a time, unless what the scan reports of rows it
            // drops says where in the file they are
            let streamed = (!state.whole
                && cached.is_none()
                && scan.rejects.is_none()
                && !scan.options.ignore_errors
                && !(state.file > 0 && scan.options.auto_detect && !scan.union_by_name))
                .then(|| streamed_size(&file.path, &scan.options))
                .flatten();
            state.stream = None;
            let data = match (cached, streamed) {
                (Some(data), _) if !state.whole => data,
                (_, Some(size)) => {
                    let buffer = buffer_size(&scan.options);
                    state.stream = Some(Stream {
                        path: file.path.clone(),
                        base: 0,
                        size,
                        buffer,
                        margin: scan.options.maximum_line_size().saturating_mul(2),
                    });
                    Arc::new(super::file_system::read_range(&file.path, 0, buffer)?)
                }
                _ => Arc::new(read_csv_file(&file.path, &scan.options)?),
            };
            let bom = if data.starts_with(&[0xef, 0xbb, 0xbf]) { 3 } else { 0 };
            state.current = if state.file > 0 && scan.options.auto_detect && !scan.union_by_name {
                Some(sniff_next_file(scan, file, &data)?)
            } else {
                None
            };
            state.file_bytes = std::fs::metadata(&file.path).map_or(data.len() as u64, |m| m.len());
            state.bytes_counted = 0;
            let file = state.current.as_ref().unwrap_or(file);
            let machine = Machine::new(file.dialect.clone(), bom);
            let rows = usize::from(file.header) + file.skip;
            let begin = skip_rows(&data, &machine.dialect, &machine.transitions(), bom, rows);
            // `CSVError::LineSizeError`: the header is a line too
            if file.header {
                let header_start = skip_rows(&data, &machine.dialect, &machine.transitions(), bom, rows - 1);
                let header = &data[header_start..begin.max(header_start)];
                let size = header.iter().rposition(|&c| c != b'\n' && c != b'\r').map_or(0, |at| at + 1);
                if size > scan.options.maximum_line_size() {
                    return Err(line_size_error(&scan.options, size));
                }
            }
            // rows to skip past the first buffer: the file is read whole
            if state
                .stream
                .as_ref()
                .is_some_and(|stream| !stream.at_end(&data) && begin + stream.margin > data.len())
            {
                state.stream = None;
                state.whole = true;
                let result = scan_file_chunk(scan, state);
                state.whole = false;
                return result;
            }
            state.line = count_lines(&data, 0, begin);
            let mut machine = machine;
            machine.position = begin;
            state.scanner = Some(ValueScanner::new(machine));
            state.data = Some(data);
        }
        let mut data = state.data.take().expect("just set");
        let current = state.current.take();
        let mut stream = state.stream.take();
        if let Some(stream) = &mut stream {
            refill(stream, &mut data, state.scanner.as_mut().expect("a scanner"), false)?;
        }
        let margin = stream
            .as_ref()
            .filter(|stream| !stream.at_end(&data))
            .map(|stream| stream.margin);
        let result = match read_rows(&data, scan, current.as_ref().unwrap_or(file), state, margin) {
            Ok(result) => result,
            Err(error) if stream.is_some() => return Err(first_error(scan, error)),
            Err(error) => return Err(error),
        };
        match result {
            Some(chunk) => {
                state.data = Some(data);
                state.current = current;
                state.stream = stream;
                count_bytes(state, buffer_size(&scan.options), false);
                return Ok(Some(chunk));
            }
            // the rest of the file, past the buffer
            None if stream.as_ref().is_some_and(|stream| !stream.at_end(&data)) => {
                let mut stream = stream.expect("just checked");
                refill(&mut stream, &mut data, state.scanner.as_mut().expect("a scanner"), true)?;
                state.data = Some(data);
                state.current = current;
                state.stream = Some(stream);
            }
            None => {
                count_bytes(state, 0, true);
                state.file += 1;
                state.stream = None;
            }
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

/// The types the scanner parses from the bytes of a value (`StringValueResult::AddValueToVector`); the others are
/// read as text and cast from it.
fn parsed_directly(ty: &LogicalType) -> bool {
    use LogicalTypeId as T;
    matches!(
        ty.id(),
        T::Boolean
            | T::TinyInt
            | T::SmallInt
            | T::Integer
            | T::BigInt
            | T::HugeInt
            | T::UTinyInt
            | T::USmallInt
            | T::UInteger
            | T::UBigInt
            | T::UHugeInt
            | T::Bignum
            | T::Double
            | T::Float
            | T::Date
            | T::Time
            | T::Timestamp
            | T::TimestampTz
            | T::Decimal
    )
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
    // `UNQUOTED VALUE`: at the value with the quote
    let (column, field_start) = match kind {
        "UNQUOTED VALUE" => unterminated_field(&data[before..end], &file.dialect)
            .map_or((0, before), |(column, start)| (column, before + start)),
        _ => (0, before),
    };
    let scanner = state.scanner.as_mut().expect("a scan has a scanner");
    scanner.skip_line(data);
    let position = scanner.machine.position;
    state.line += count_lines(data, before, position);
    reject(
        scan,
        state,
        Reject {
            file: state.file,
            line,
            row_position: before + 1,
            position: Some(field_start + 1),
            column: Some(column + 1),
            column_name: scan.names.get(column).cloned(),
            kind,
            text,
            message,
        },
    );
    Ok(())
}

/// The value of a line that has a quote it does not end on (`"a"b`): its column, and where it starts.
fn unterminated_field(line: &[u8], dialect: &Dialect) -> Option<(usize, usize)> {
    let quote = dialect.quote?;
    let delimiter = dialect.delimiter.as_slice();
    if delimiter.is_empty() {
        return None;
    }
    let (mut column, mut start, mut at) = (0, 0, 0);
    while at <= line.len() {
        if line.get(at) == Some(&quote) && at == start {
            // a quoted value: to its closing quote, which must be followed by a delimiter or the end
            let mut end = at + 1;
            while end < line.len() {
                if line[end] == quote {
                    if line.get(end + 1) == Some(&quote) {
                        end += 2;
                        continue;
                    }
                    break;
                }
                end += 1;
            }
            let after = end + 1;
            if end >= line.len() || (after < line.len() && !line[after..].starts_with(delimiter)) {
                return Some((column, start));
            }
            at = after;
            continue;
        }
        if at == line.len() {
            break;
        }
        if line[at..].starts_with(delimiter) {
            column += 1;
            at += delimiter.len();
            start = at;
            continue;
        }
        at += 1;
    }
    None
}

/// The next rows of `data`, typed; `None` past its last. With a `margin`, a row starts only with that many bytes of
/// `data` after it, as `data` is a buffer of a file that goes on.
fn read_rows(
    data: &[u8],
    scan: &CsvScanData,
    file: &CsvFile,
    state: &mut CsvScanState,
    margin: Option<usize>,
) -> Result<Option<DataChunk>> {
    let ignore = scan.options.ignore_errors;
    let padding = scan.options.null_padding;
    let strict = file.dialect.strict;
    let empty_is_null = scan.options.null_strs.iter().any(String::is_empty);
    loop {
        let mut rows: Vec<Vec<Field<'_>>> = Vec::with_capacity(CHUNK_ROWS);
        let mut spans = Vec::with_capacity(CHUNK_ROWS);
        let mut doomed = Vec::with_capacity(CHUNK_ROWS);
        // the errors of these rows go in by line, those of a line together, as the scanner meets them
        let rejects_start = state.rejects.len();
        while rows.len() < CHUNK_ROWS {
            let scanner = state.scanner.as_mut().expect("a scan has a scanner");
            let before = scanner.machine.position;
            if margin.is_some_and(|margin| data.len() - before < margin) {
                break;
            }
            let row = match scanner.next(data) {
                None => break,
                Some(Scanned::Row(row)) => row,
                Some(Scanned::EmptyLine) => {
                    let position = scanner.machine.position;
                    state.line += count_lines(data, before, position);
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
                line: state.line + count_lines(data, before, row.start.max(before)) + 1,
                start: row.start,
                end: row.end,
            };
            state.line += count_lines(data, before, position);
            let line_size = position.saturating_sub(span.start);
            let mut doomed_row = false;
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
                // what the line has is still checked: its other errors are reported too
                doomed_row = true;
            }
            let invalid: Vec<usize> = (0..row_fields.len())
                .filter(|&f| std::str::from_utf8(&row_fields[f].text).is_err())
                .collect();
            if !invalid.is_empty() {
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
                // a report per value of a column read as text (`ParseTypeInfo`: VARCHAR, and what is cast from it, such
                // as the nested types); one of a type the scanner parses fails that parse, which is reported then
                let text = span.text(data);
                for field in invalid {
                    let column = file.mapping.iter().position(|m| *m == Some(field));
                    let varchar = column.is_none_or(|c| scan.types.get(c).is_none_or(|ty| !parsed_directly(ty)));
                    if varchar {
                        // (reported once: what is left of the value reads as NULL, and is not cast again)
                        if let Some(null) = scan.options.null_strs.first().filter(|n| !n.starts_with('\\')) {
                            row_fields[field].text = std::borrow::Cow::Owned(null.as_bytes().to_vec());
                            row_fields[field].quoted = false;
                            row_fields[field].escaped = false;
                        }
                        reject(
                            scan,
                            state,
                            Reject {
                                file: state.file,
                                line: span.line,
                                row_position: span.start + 1,
                                position: Some(row_fields[field].offset + 1),
                                column: Some(field + 1),
                                column_name: column.and_then(|c| scan.names.get(c)).cloned(),
                                kind: "INVALID ENCODING",
                                text: text.clone(),
                                message: message.to_string(),
                            },
                        );
                    }
                    if std::str::from_utf8(&row_fields[field].text).is_err() {
                        let valid = make_valid(&row_fields[field].text, '?');
                        row_fields[field].text = std::borrow::Cow::Owned(valid.into_bytes());
                    }
                }
                doomed_row = true;
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
            doomed.push(doomed_row);
        }
        if rows.is_empty() {
            return Ok(None);
        }
        loop {
            match typed_chunk(data, &rows, scan, file, &spans, &doomed, state)? {
                Typed::Chunk(chunk) => {
                    state.rejects[rejects_start..].sort_by_key(|reject| (reject.file, reject.line));
                    return Ok(Some(chunk));
                }
                Typed::Reject(bad) => {
                    for &row in bad.iter().rev() {
                        rows.remove(row);
                        spans.remove(row);
                        doomed.remove(row);
                    }
                    if rows.is_empty() {
                        state.rejects[rejects_start..].sort_by_key(|reject| (reject.file, reject.line));
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
        LogicalTypeId::Timestamp | LogicalTypeId::TimestampTz => parsing.timestamp_format.as_deref(),
        _ => None,
    };
    if let Some(format) = format {
        // `CSVCast::TryCastDateOperator` / `TryCastTimestampOperator`: the time the format parses, as UTC
        let mut result = Vector::new(ty.clone(), count);
        for row in 0..count {
            let Some(bytes) = strings.str_at(row) else {
                result.set_null(row, true);
                continue;
            };
            let text = std::str::from_utf8(bytes).map_err(|e| Error::new(ErrorKind::Conversion, e.to_string()))?;
            let micros = strptime_micros(text, format).map_err(|e| Error::new(ErrorKind::Conversion, e))?;
            if ty.id() == LogicalTypeId::Date {
                result.data_mut::<i32>()[row] = Timestamp(micros).date().0;
            } else {
                result.data_mut::<i64>()[row] = micros;
            }
        }
        return Ok(Some(result));
    }
    let numeric = ty.id() == LogicalTypeId::Bignum
        || (ty.is_numeric() && (parsing.thousands.is_some() || parsing.decimal_separator != b'.'));
    if !numeric {
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
        let converted = {
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
        // a column nothing reads is not converted (`column_ids` of the scan)
        if scan
            .projection
            .as_ref()
            .is_some_and(|columns| !columns.contains(&index))
        {
            result.push(Vector::constant_null(ty.clone()));
            continue;
        }
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
                    // (a value read as text and cast after has no position of its own)
                    position: fields[row].filter(|_| parsed_directly(ty)).map(|field| field.offset + 1),
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
    table.write_ahead_when_full()
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
                text(r.file + rejects.file_offset),
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
            .map(|(name, ty)| {
                format!(
                    "'{}': '{}'",
                    name.replace('\'', "''"),
                    ty.to_string().replace('\'', "''")
                )
            })
            .collect();
        let new_line = new_line_identifier(&file.dialect);
        scans.push(vec![
            Some(rejects.scan_id.to_string()),
            text(index + rejects.file_offset),
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
