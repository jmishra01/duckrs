// Ported from duckdb/src/planner/binder/statement/bind_copy.cpp,
// duckdb/src/function/table/copy_csv.cpp and duckdb/src/function/table/read_csv.cpp
// @ rust-port-baseline
//
//! `read_csv` and `COPY`. COPY FROM is an INSERT from `read_csv`, as C++ plans it. Only CSV is here
//! so far; other formats, and options that aren't, fail as not ported rather than being ignored.

use super::expression::enum_type_from;
use super::table_functions::{named_argument, parse_select};
use super::{Binder, Binding, BoundStatement, StatementReturnType, binder_error};
use crate::catalog::{ColumnDefinition, OnCreateConflict, TableCatalogEntry, TableOrView};
use crate::execution::csv::{
    CsvFile, CsvReadOptions, CsvRejects, CsvScanData, CsvWriteOptions, hive_parts, new_line_identifier, read_csv_file,
};
use crate::execution::csv_sniffer::{Parsing, Sniffed, can_cast, sniff};
use crate::execution::csv_state_machine::NewLine;
use crate::execution::file_system::{self, Compression};
use crate::execution::parquet::Partitioning;
use crate::function::combine_types::force_max_logical_type;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    CopyInfo, Expr, FunctionExpr, InsertStatement, InsertValues, SelectStatement, TableRef, sql_identifier,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) fn is_true(value: &Value, name: &str) -> Result<bool> {
    match value.data() {
        ValueData::Bool(flag) => Ok(*flag),
        _ if value.is_null() => Err(binder_error(format!("{name} cannot be NULL"))),
        _ => match value.try_to_string()?.to_lowercase().as_str() {
            "1" | "true" | "on" => Ok(true),
            "0" | "false" | "off" => Ok(false),
            _ => Err(binder_error(format!("\"{name}\" expected an argument of type BOOLEAN"))),
        },
    }
}

/// The `filename` option of the multi-file readers (`MultiFileReader::ParseOption`): a VARCHAR names
/// the column, anything else is cast to BOOLEAN.
pub(super) fn filename_option(value: &Value, option: &str) -> Result<Option<String>> {
    if value.logical_type().id() == LogicalTypeId::Varchar {
        return Ok(Some(value.try_to_string()?));
    }
    Ok(is_true(value, option)?.then(|| "filename".to_string()))
}

fn text(value: &Value) -> Result<String> {
    value.try_to_string()
}

/// One byte, or none for the empty string.
fn single_byte(value: &Value, name: &str) -> Result<Option<u8>> {
    let text = if value.is_null() { String::new() } else { text(value)? };
    match text.as_bytes() {
        [] => Ok(None),
        [byte] => Ok(Some(*byte)),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("The {name} option cannot exceed a size of 1 byte."),
        )),
    }
}

/// `BaseCSVData::Finalize`: what must not be part of what else.
fn csv_finalize(options: &CsvReadOptions) -> Result<()> {
    let conflict =
        |a: &str, b: &str| binder_error(format!("{a} must not appear in the {b} specification and vice versa"));
    let delimiter = options.delimiter.clone().unwrap_or_default();
    let quote = options.quote.flatten();
    let escape = options.escape.flatten();
    let comment = options.comment.flatten();
    let inside = |byte: Option<u8>, text: &[u8]| byte.is_some_and(|b| text.contains(&b));
    if options.quote.is_some() && inside(quote, &delimiter) {
        return Err(conflict("QUOTE", "DELIMITER"));
    }
    if options.escape.is_some() && inside(escape, &delimiter) {
        return Err(conflict("ESCAPE", "DELIMITER"));
    }
    if inside(comment, &delimiter) {
        return Err(conflict("COMMENT", "DELIMITER"));
    }
    if comment.is_some() && comment == quote {
        return Err(conflict("COMMENT", "QUOTE"));
    }
    if options.thousands.is_some() && options.thousands == Some(options.decimal_separator) {
        return Err(conflict("THOUSANDS", "DECIMAL_SEPARATOR"));
    }
    for null in options.null_strs.iter().filter(|n| !n.is_empty()) {
        let bytes = null.as_bytes();
        if !delimiter.is_empty() && bytes.windows(delimiter.len()).any(|w| w == delimiter.as_slice()) {
            return Err(conflict("DELIMITER", "NULL"));
        }
        if inside(quote, bytes) {
            return Err(conflict("QUOTE", "NULL"));
        }
        if !options.strict_mode && bytes.len() == 2 && Some(bytes[0]) == escape {
            continue;
        }
        if inside(escape, bytes) {
            return Err(conflict("ESCAPE", "NULL"));
        }
    }
    Ok(())
}

/// `CSVReaderOptions::SetDelimiter`
fn read_delimiter(value: &Value) -> Result<Vec<u8>> {
    let result = delimiter(value)?;
    if result.len() > 4 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "The delimiter option cannot exceed a size of 4 bytes.",
        ));
    }
    Ok(result)
}

fn delimiter(value: &Value) -> Result<Vec<u8>> {
    let text = if value.is_null() { String::new() } else { text(value)? };
    let text = if text == "\\t" { "\t".to_string() } else { text };
    if text.is_empty() {
        return Ok(vec![0]);
    }
    Ok(text.into_bytes())
}

pub(super) fn strings(value: &Value) -> Result<Vec<String>> {
    match value.data() {
        ValueData::List(items) | ValueData::Struct(items) => items.iter().map(text).collect(),
        _ => Ok(vec![text(value)?]),
    }
}

/// A newline as C++'s `TransformNewLine` reads it.
fn new_line(value: &str) -> Result<String> {
    match value.to_lowercase().as_str() {
        "\\n" | "\n" | "lf" => Ok("\n".to_string()),
        "\\r\\n" | "\r\n" | "crlf" => Ok("\r\n".to_string()),
        "\\r" | "\r" | "cr" => Ok("\r".to_string()),
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Version {value} is not a valid value for new_line"),
        )),
    }
}

/// The arguments of `read_csv`.
#[derive(Default)]
struct CsvArguments {
    files: Vec<String>,
    options: CsvReadOptions,
    auto_detect: Option<bool>,
    columns: Option<Vec<(String, LogicalType)>>,
    names: Option<Vec<String>>,
    types_by_name: Vec<(String, LogicalType)>,
    types_by_position: Vec<LogicalType>,
    union_by_name: bool,
    /// `files_to_sniff`: how many files the schema is taken from
    files_to_sniff: usize,
    allow_empty: bool,
    hive: HiveOptions,
    filename: Option<String>,
    store_rejects: Option<bool>,
    storing: bool,
    rejects_table: Option<String>,
    rejects_scan: Option<String>,
    rejects_limit: usize,
    rejects_line_size_limit: usize,
    /// `user_defined_parameters`: name and SQL text, as `sniff_csv` shows them
    user_arguments: Vec<(String, String)>,
}

impl Binder<'_> {
    /// The type that `name` spells, resolved as a cast to it would be.
    pub(super) fn csv_type(&mut self, name: &str) -> Result<LogicalType> {
        if name.eq_ignore_ascii_case("null") {
            return Ok(LogicalType::SQLNULL);
        }
        // `TransformStringToUnboundType`: text that isn't a type, with the types it is most like
        let select = parse_select(&format!("SELECT NULL::{name}"), "Expected a type").map_err(|error| {
            if error.kind != ErrorKind::Parser {
                return error;
            }
            const SUGGESTED: &[&str] = &[
                "BIGINT",
                "INT8",
                "LONG",
                "BIT",
                "BITSTRING",
                "BLOB",
                "BYTEA",
                "BINARY,",
                "VARBINARY",
                "BOOLEAN",
                "BOOL",
                "LOGICAL",
                "DATE",
                "DECIMAL(prec, scale)",
                "DOUBLE",
                "FLOAT8",
                "FLOAT",
                "FLOAT4",
                "REAL",
                "HUGEINT",
                "INTEGER",
                "INT4",
                "INT",
                "SIGNED",
                "INTERVAL",
                "SMALLINT",
                "INT2",
                "SHORT",
                "TIME",
                "TIMESTAMPTZ",
                "TIMESTAMP",
                "DATETIME",
                "TINYINT",
                "INT1",
                "UBIGINT",
                "UHUGEINT",
                "UINTEGER",
                "USMALLINT",
                "UTINYINT",
                "UUID",
                "VARCHAR",
                "CHAR",
                "BPCHAR",
                "TEXT",
                "STRING",
                "MAP(INTEGER, VARCHAR)",
                "UNION(num INTEGER, text VARCHAR)",
            ];
            let scores = SUGGESTED
                .iter()
                .map(|t| (t.to_string(), duckrs_common::string_util::similarity_rating(t, name)))
                .collect();
            let mut message = format!(
                "Value \"{name}\" can not be converted to a DuckDB Type.\nPossible examples as suggestions: \n"
            );
            for suggestion in duckrs_common::string_util::top_n_strings(scores) {
                message += &format!("* {suggestion}\n");
            }
            Error::new(ErrorKind::InvalidInput, message)
        })?;
        let bound = self.child_binder().bind_select_statement(&select)?;
        bound
            .types
            .into_iter()
            .next()
            .ok_or_else(|| binder_error("Expected a type"))
    }

    fn csv_fields(&mut self, value: &Value, name: &str) -> Result<Vec<(String, LogicalType)>> {
        let ValueData::Struct(fields) = value.data() else {
            return Err(binder_error(format!("{name} requires a struct as input.")));
        };
        let keys = value.logical_type().struct_children().to_vec();
        let mut result = Vec::with_capacity(fields.len());
        let quoted = if name == "columns" {
            name.to_string()
        } else {
            format!("\"{name}\"")
        };
        for ((key, _), field) in keys.iter().zip(fields) {
            if field.is_null() {
                return Err(binder_error(format!("read_csv {quoted} cannot have a NULL type")));
            }
            if field.logical_type().id() != LogicalTypeId::Varchar {
                return Err(binder_error("read_csv requires a type specification as string"));
            }
            result.push((key.clone(), self.csv_type(&text(field)?)?));
        }
        Ok(result)
    }

    /// `auto_type_candidates`: VARCHAR and the given types, from the least specific.
    fn csv_type_candidates(&mut self, names: &[String]) -> Result<Vec<LogicalType>> {
        // `type_id_to_specificity`
        let specificity = |ty: &LogicalType| match ty.id() {
            LogicalTypeId::Varchar => Some(0),
            LogicalTypeId::Double => Some(1),
            LogicalTypeId::Float => Some(2),
            LogicalTypeId::Decimal => Some(3),
            LogicalTypeId::Bignum => Some(4),
            LogicalTypeId::HugeInt => Some(5),
            LogicalTypeId::BigInt => Some(6),
            LogicalTypeId::Integer => Some(7),
            LogicalTypeId::SmallInt => Some(8),
            LogicalTypeId::TinyInt => Some(9),
            LogicalTypeId::TimestampTz => Some(10),
            LogicalTypeId::Timestamp => Some(11),
            LogicalTypeId::Date => Some(12),
            LogicalTypeId::Time => Some(13),
            LogicalTypeId::Boolean => Some(14),
            LogicalTypeId::SqlNull => Some(15),
            _ => None,
        };
        let mut found = std::collections::BTreeMap::new();
        found.insert(0, LogicalType::VARCHAR);
        found.insert(15, LogicalType::SQLNULL);
        for name in names {
            let ty = self.csv_type(name)?;
            let Some(rank) = specificity(&ty) else {
                let shown = ty.to_string();
                let shown = shown.split('(').next().unwrap_or_default();
                return Err(binder_error(format!(
                    "Auto Type Candidate of type {shown} is not accepted as a valid input"
                )));
            };
            found.insert(rank, ty);
        }
        Ok(found.into_values().collect())
    }

    /// `CSVReaderOptions::FromNamedParameters`.
    fn csv_arguments(&mut self, name: &str, call: &FunctionExpr) -> Result<CsvArguments> {
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        let mut result = CsvArguments {
            files_to_sniff: 10,
            ..CsvArguments::default()
        };
        let mut paths = None;
        let mut ignore_given = None;
        for argument in &args {
            let value = self.constant_argument(&argument.expr)?;
            if argument.name.is_empty() {
                if paths.is_some() {
                    return Err(binder_error(format!("{name} takes one positional parameter")));
                }
                if value.is_null() {
                    return Err(binder_error(format!("\"{name}\" cannot take NULL list as parameter")));
                }
                paths = Some(strings(&value)?);
                continue;
            }
            let option = argument.name.to_lowercase();
            let options = &mut result.options;
            let mut shown = true;
            match option.as_str() {
                "delim" | "sep" | "delimiter" => {
                    if options.delimiter.is_some() {
                        return Err(binder_error(
                            "CSV Reader function option delim and sep are aliases, only one can be supplied",
                        ));
                    }
                    options.delimiter = Some(read_delimiter(&value)?);
                }
                "quote" => options.quote = Some(single_byte(&value, "quote")?),
                "escape" => options.escape = Some(single_byte(&value, "escape")?),
                "comment" => options.comment = Some(single_byte(&value, "comment")?),
                "header" => options.header = Some(is_true(&value, &option)?),
                "skip" => {
                    let skip: i64 = text(&value)?
                        .parse()
                        .map_err(|_| binder_error("skip must be an integer"))?;
                    if skip < 0 {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "skip_rows option from read_csv scanner, must be equal or higher than 0",
                        ));
                    }
                    options.skip = Some(skip as usize);
                }
                "nullstr" | "null" => {
                    let ty = value.logical_type().id();
                    if !matches!(
                        ty,
                        LogicalTypeId::List | LogicalTypeId::Varchar | LogicalTypeId::SqlNull
                    ) {
                        return Err(binder_error(format!(
                            "CSV Reader function option \"{option}\" requires a string or a list as input"
                        )));
                    }
                    options.null_strs = strings(&value)?;
                    if ty == LogicalTypeId::List && options.null_strs.is_empty() {
                        return Err(binder_error(format!(
                            "CSV Reader function option \"{option}\" requires a non-empty list of possible null \
                             strings (varchar) as input"
                        )));
                    }
                }
                "auto_detect" => {
                    shown = false;
                    result.auto_detect = Some(is_true(&value, &option)?);
                }
                "all_varchar" => options.all_varchar = is_true(&value, &option)?,
                "ignore_errors" => options.ignore_errors = is_true(&value, &option)?,
                "null_padding" => options.null_padding = is_true(&value, &option)?,
                "allow_quoted_nulls" => options.allow_quoted_nulls = is_true(&value, &option)?,
                "sample_size" => {
                    let size: i64 = text(&value)?
                        .parse()
                        .map_err(|_| binder_error("sample_size must be an integer"))?;
                    if size < 1 && size != -1 {
                        return Err(binder_error(
                            "Unsupported parameter for SAMPLE_SIZE: cannot be smaller than 1",
                        ));
                    }
                    options.sample_size = if size < 0 { usize::MAX } else { size as usize };
                }
                // how C++ reads, not what it reads
                "parallel" => {}
                "buffer_size" => {
                    let size: i64 = text(&value)?
                        .parse()
                        .map_err(|_| binder_error("buffer_size must be an integer"))?;
                    if size <= 0 {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "Buffer Size option must be higher than 0",
                        ));
                    }
                    let size = size as usize;
                    if let Some(line) = options.max_line_size.filter(|line| *line > size) {
                        return Err(buffer_below_line_size(size, line));
                    }
                    options.buffer_size = Some(size);
                }
                "maximum_line_size" | "max_line_size" => {
                    let size: i64 = text(&value)?
                        .parse()
                        .map_err(|_| binder_error("max_line_size must be an integer"))?;
                    if size < 0 {
                        return Err(binder_error(
                            "Invalid value for MAX_LINE_SIZE parameter: it cannot be smaller than 0",
                        ));
                    }
                    let size = size as usize;
                    if let Some(buffer) = options.buffer_size.filter(|buffer| size > *buffer) {
                        return Err(buffer_below_line_size(buffer, size));
                    }
                    // `CSVReaderOptions::Verify`: a larger line size makes the buffers larger
                    if options.buffer_size.is_none() && size > 2_000_000 && size.checked_mul(16).is_none() {
                        return Err(binder_error(format!(
                            "MAX_LINE_SIZE option was set to {size}, which is too large"
                        )));
                    }
                    options.max_line_size = Some(size);
                }
                "strict_mode" => options.strict_mode = is_true(&value, &option)?,
                "new_line" => {
                    options.new_line = Some(match text(&value)?.as_str() {
                        "\\n" => NewLine::SingleN,
                        "\\r" => NewLine::SingleR,
                        "\\r\\n" => NewLine::CarryOn,
                        other => {
                            return Err(Error::new(
                                ErrorKind::InvalidInput,
                                format!("This is not accepted as a newline: {other}"),
                            ));
                        }
                    });
                }
                "dateformat" | "date_format" => {
                    let format = text(&value)?;
                    if !format.eq_ignore_ascii_case("auto") {
                        options.date_format = Some(format);
                    }
                }
                "timestampformat" | "timestamp_format" => {
                    let format = text(&value)?;
                    if !format.eq_ignore_ascii_case("auto") {
                        options.timestamp_format = Some(format);
                    }
                }
                "decimal_separator" => {
                    let separator = text(&value)?;
                    if !matches!(separator.as_str(), "." | ",") {
                        return Err(binder_error(
                            "Unsupported parameter for DECIMAL_SEPARATOR: should be '.' or ','",
                        ));
                    }
                    options.decimal_separator = separator.as_bytes()[0];
                }
                "thousands" => {
                    let separator = if value.is_null() { String::new() } else { text(&value)? };
                    if separator.len() > 1 {
                        return Err(binder_error(
                            "Unsupported parameter for THOUSANDS: should be max one character",
                        ));
                    }
                    options.thousands = separator.bytes().next();
                }
                "normalize_names" => options.normalize_names = is_true(&value, &option)?,
                "auto_type_candidates" => {
                    shown = false;
                    if value.logical_type().id() != LogicalTypeId::List {
                        return Err(binder_error("read_csv auto_types requires a list as input"));
                    }
                    if value.is_null() {
                        return Err(binder_error("auto_type_candidates cannot be NULL"));
                    }
                    let names = strings(&value)?;
                    if names.is_empty() {
                        return Err(binder_error("auto_type_candidates requires at least one type"));
                    }
                    if matches!(value.data(), ValueData::List(items) if items.iter().any(Value::is_null)) {
                        return Err(binder_error("auto_type_candidates cannot contain NULL types"));
                    }
                    options.auto_type_candidates = Some(self.csv_type_candidates(&names)?);
                }
                "force_not_null" => options.force_not_null = strings(&value)?,
                "encoding" => options.encoding = Some(text(&value)?),
                "files_to_sniff" => {
                    let files: i64 = text(&value)?
                        .parse()
                        .map_err(|_| binder_error("files_to_sniff must be an integer"))?;
                    if files < 1 && files != -1 {
                        return Err(binder_error(
                            "Unsupported parameter for files_to_sniff: value must be -1 for all files or higher than \
                             one.",
                        ));
                    }
                    result.files_to_sniff = if files == -1 { usize::MAX } else { files as usize };
                }
                "allow_empty" => {
                    shown = false;
                    result.allow_empty = is_true(&value, &option)?;
                }
                "hive_partitioning" | "hive_types" | "hive_type" | "hive_types_autocast" | "hive_type_autocast" => {
                    shown = false;
                    let mut hive = std::mem::take(&mut result.hive);
                    self.take_hive_option(&mut hive, &option, &value)?;
                    result.hive = hive;
                }
                "compression" => options.compression = Compression::parse(&text(&value)?)?,
                "columns" => {
                    shown = false;
                    if result.names.is_some() {
                        return Err(binder_error("read_csv column_names/names can only be supplied once"));
                    }
                    if value.logical_type().id() != LogicalTypeId::Struct {
                        return Err(binder_error("read_csv columns requires a struct as input"));
                    }
                    if value.is_null() {
                        return Err(binder_error("read_csv columns cannot be NULL"));
                    }
                    result.columns = Some(self.csv_fields(&value, "columns")?);
                }
                "names" | "column_names" => {
                    shown = false;
                    if result.names.is_some() || result.columns.is_some() {
                        return Err(binder_error("read_csv column_names/names can only be supplied once"));
                    }
                    if value.is_null() {
                        return Err(binder_error(format!("read_csv \"{option}\" cannot be NULL")));
                    }
                    let names = strings(&value)?;
                    let mut seen = std::collections::HashSet::new();
                    for name in &names {
                        if name.trim().is_empty() {
                            return Err(binder_error(format!(
                                "read_csv \"{option}\" cannot have empty (or all whitespace) value"
                            )));
                        }
                        if !seen.insert(name.to_lowercase()) {
                            return Err(binder_error(format!(
                                "read_csv \"{option}\" must have unique values. {name} is repeated."
                            )));
                        }
                    }
                    result.names = Some(names);
                }
                "types" | "dtypes" | "column_types" => {
                    shown = false;
                    let id = value.logical_type().id();
                    if !matches!(id, LogicalTypeId::Struct | LogicalTypeId::List) {
                        return Err(binder_error(format!(
                            "read_csv \"{option}\" requires a struct or list as input"
                        )));
                    }
                    if !result.types_by_name.is_empty() || !result.types_by_position.is_empty() {
                        return Err(binder_error(
                            "read_csv column_types/types/dtypes can only be supplied once",
                        ));
                    }
                    if value.is_null() {
                        return Err(binder_error(format!("read_csv \"{option}\" cannot be NULL")));
                    }
                    if id == LogicalTypeId::Struct {
                        result.types_by_name = self.csv_fields(&value, &option)?;
                    } else {
                        let ValueData::List(items) = value.data() else {
                            return Err(binder_error(format!(
                                "read_csv \"{option}\" requires a list of types (varchar) as input"
                            )));
                        };
                        let mut types = Vec::with_capacity(items.len());
                        for item in items {
                            if item.is_null() {
                                return Err(binder_error(format!("read_csv \"{option}\" cannot have a NULL type")));
                            }
                            if item.logical_type().id() != LogicalTypeId::Varchar {
                                return Err(binder_error(format!(
                                    "read_csv \"{option}\" requires a list of types (varchar) as input"
                                )));
                            }
                            types.push(self.csv_type(&text(item)?)?);
                        }
                        result.types_by_position = types;
                    }
                }
                "union_by_name" => {
                    result.union_by_name = is_true(&value, &option)?;
                    shown = false;
                }
                "filename" => {
                    shown = false;
                    result.filename = filename_option(&value, &option)?;
                }
                "force_match" if name == "sniff_csv" => shown = false,
                "store_rejects" => result.store_rejects = Some(is_true(&value, &option)?),
                "rejects_table" => {
                    let table = text(&value)?;
                    if table.is_empty() {
                        return Err(binder_error("REJECTS_TABLE option cannot be empty"));
                    }
                    result.rejects_table = Some(table);
                }
                "rejects_scan" => {
                    let table = text(&value)?;
                    if table.is_empty() {
                        return Err(binder_error("rejects_scan option cannot be empty"));
                    }
                    result.rejects_scan = Some(table);
                }
                "rejects_line_size_limit" => {
                    result.rejects_line_size_limit = text(&value)?
                        .parse()
                        .map_err(|_| binder_error("rejects_line_size_limit must be a non-negative integer"))?;
                }
                "rejects_limit" => {
                    let limit: i64 = text(&value)?
                        .parse()
                        .map_err(|_| binder_error("rejects_limit must be an integer"))?;
                    if limit < 0 {
                        return Err(binder_error(
                            "Unsupported parameter for REJECTS_LIMIT: cannot be negative",
                        ));
                    }
                    result.rejects_limit = limit as usize;
                }
                _ => return Err(crate::unported(format!("the {name} option {option}"))),
            }
            if option == "ignore_errors" {
                ignore_given = Some(result.options.ignore_errors);
            }
            if shown {
                result.user_arguments.push((option, value_literal(&value)?));
            }
        }
        // `VerifyTypeListsMatch`: with `columns`, the types of `types` must be those of the columns
        if let Some(columns) = &result.columns {
            let given: Vec<LogicalType> = if result.types_by_position.is_empty() {
                result.types_by_name.iter().map(|(_, ty)| ty.clone()).collect()
            } else {
                result.types_by_position.clone()
            };
            if !given.is_empty() {
                if given.len() != columns.len() {
                    return Err(binder_error(format!(
                        "read_csv: the 'columns' option specifies {} column(s), but 'types'/'dtypes'/'column_types' \
                         specifies {} type(s). When both are provided they must agree. Consider removing the 'type' \
                         option.",
                        columns.len(),
                        given.len()
                    )));
                }
                for (at, ((_, column), ty)) in columns.iter().zip(&given).enumerate() {
                    if column != ty {
                        return Err(binder_error(format!(
                            "read_csv: column type mismatch at position {}: 'columns' specifies '{column}' but \
                             'types'/'dtypes'/'column_types' specifies '{ty}'. When both are provided they must \
                             agree. Consider removing the 'type' option.",
                            at + 1
                        )));
                    }
                }
                result.types_by_position.clear();
                result.types_by_name.clear();
            }
        }
        csv_finalize(&result.options)?;
        // `CSVReaderOptions::SetRejectsOptions`
        let named = result.rejects_table.is_some() || result.rejects_scan.is_some();
        if named && result.store_rejects == Some(false) {
            return Err(binder_error(if result.rejects_table.is_some() {
                "REJECTS_TABLE option is only supported when store_rejects is not manually set to false"
            } else {
                "REJECTS_SCAN option is only supported when store_rejects is not manually set to false"
            }));
        }
        result.storing = result.store_rejects.unwrap_or(named);
        if result.storing {
            if ignore_given == Some(false) {
                return Err(binder_error(
                    "STORE_REJECTS option is only supported when IGNORE_ERRORS is not manually set to false",
                ));
            }
            result.options.ignore_errors = true;
            if result.union_by_name {
                return Err(binder_error(
                    "REJECTS_TABLE option is not supported when UNION_BY_NAME is set to true",
                ));
            }
        }
        if result.rejects_limit != 0 && !result.storing {
            return Err(binder_error(
                "REJECTS_LIMIT option is only supported when REJECTS_TABLE is set to a table name",
            ));
        }
        let patterns = paths.ok_or_else(|| binder_error(format!("{name} requires a file name")))?;
        for pattern in &patterns {
            self.check_file_access(pattern)?;
            let found = file_system::glob_or_directory(pattern, "csv");
            if found.is_empty() && !result.allow_empty {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            result.files.extend(found);
        }
        // `MultiFileReader::CreateFileList`: an empty list names no file
        if result.files.is_empty() && !result.allow_empty {
            return Err(Error::new(ErrorKind::Io, format!("{name} needs at least one file to read")));
        }
        result.options.auto_detect = result.auto_detect.unwrap_or(true);
        Ok(result)
    }

    /// What sniffing says of a file, with what the user decided put over it.
    fn csv_sniff(arguments: &CsvArguments, path: &str) -> Result<(Sniffed, Vec<u8>)> {
        let data = read_csv_file(path, &arguments.options)?;
        let fixed: Option<(Vec<String>, Vec<LogicalType>)> = arguments
            .columns
            .as_ref()
            .map(|columns| columns.iter().cloned().unzip());
        let mut sniffed = sniff(
            &data,
            path,
            &arguments.options,
            fixed
                .as_ref()
                .map(|(names, types)| (names.as_slice(), types.as_slice())),
        )?;
        if let Some(names) = &arguments.names {
            if names.len() > sniffed.names.len() {
                return Err(binder_error(
                    "read_csv: names has more entries than the file has columns",
                ));
            }
            sniffed.names[..names.len()].clone_from_slice(names);
        }
        for (i, ty) in arguments.types_by_position.iter().enumerate() {
            match sniffed.types.get_mut(i) {
                Some(slot) => *slot = ty.clone(),
                None => {
                    return Err(binder_error(format!(
                        "read_csv: {} types were provided, but CSV file only has {} columns",
                        arguments.types_by_position.len(),
                        sniffed.names.len()
                    )));
                }
            }
        }
        for (column, ty) in &arguments.types_by_name {
            let Some(i) = sniffed.names.iter().position(|n| n.eq_ignore_ascii_case(column)) else {
                // with `union_by_name` a column only has to be in one of the files
                if arguments.union_by_name {
                    continue;
                }
                return Err(missing_column_types(column));
            };
            sniffed.types[i] = ty.clone();
        }
        // `SetResultOptions`
        if !sniffed.types.iter().any(|ty| ty.id() == LogicalTypeId::Date) {
            sniffed.date_format = arguments.options.date_format.clone();
        }
        if !sniffed.types.iter().any(|ty| ty.id() == LogicalTypeId::Timestamp) {
            sniffed.timestamp_format = arguments.options.timestamp_format.clone();
        }
        Ok((sniffed, data))
    }

    /// `CSVSchemaDiscovery::SchemaDiscovery`: the files after the first are sniffed until enough lines are, or
    /// `files_to_sniff` files, and the types of those with rows are merged into the first's.
    fn merge_sniffed_schemas(arguments: &CsvArguments, first: &Sniffed, types: &mut [LogicalType]) -> Result<()> {
        let rows_read = |sniffed: &Sniffed| {
            sniffed
                .lines_sniffed
                .saturating_sub(sniffed.skip + usize::from(sniffed.header))
        };
        let required = arguments.options.sample_size;
        let mut total = first.lines_sniffed;
        let mut best_rows = rows_read(first);
        let count = arguments.files.len().min(arguments.files_to_sniff);
        for path in arguments.files.iter().take(count).skip(1) {
            if total >= required {
                break;
            }
            let (file, data) = Self::csv_sniff(arguments, path)?;
            total += file.lines_sniffed;
            let rows = rows_read(&file);
            if data.is_empty() || rows == 0 {
                continue;
            }
            if best_rows == 0 {
                // a schema without data rows gives way to one that has them
                for (slot, ty) in types.iter_mut().zip(&file.types) {
                    *slot = ty.clone();
                }
                best_rows = rows;
                continue;
            }
            merge_schema_types(types, &file.types);
        }
        Ok(())
    }

    /// `ReadCSVBind`.
    pub(super) fn bind_read_csv(&mut self, call: &FunctionExpr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let arguments = self.csv_arguments(&name, call)?;
        if arguments.files.is_empty() {
            return self.bind_empty_csv(table_ref, &name, arguments);
        }
        let (first, first_data) = Self::csv_sniff(&arguments, &arguments.files[0])?;
        let (mut names, mut types) = (first.names.clone(), first.types.clone());
        let first_formats = (first.date_format.clone(), first.timestamp_format.clone());
        let first_mismatch = first.mismatch.clone();
        let mut files = Vec::with_capacity(arguments.files.len());
        if arguments.union_by_name {
            // `UnionByName::UnionCols`: all the columns, each with a type all its files fit
            let mut sniffed = vec![first];
            for path in &arguments.files[1..] {
                let (file, _) = Self::csv_sniff(&arguments, path)?;
                for (column, ty) in file.names.iter().zip(&file.types) {
                    match names.iter().position(|n| n.eq_ignore_ascii_case(column)) {
                        Some(i) => types[i] = force_max_logical_type(&types[i], ty),
                        None => {
                            names.push(column.clone());
                            types.push(ty.clone());
                        }
                    }
                }
                sniffed.push(file);
            }
            if let Some((column, _)) = arguments
                .types_by_name
                .iter()
                .find(|(column, _)| !names.iter().any(|n| n.eq_ignore_ascii_case(column)))
            {
                return Err(missing_column_types(column));
            }
            for ty in &mut types {
                if ty.id() == LogicalTypeId::SqlNull {
                    *ty = LogicalType::VARCHAR;
                }
            }
            for (path, file) in arguments.files.iter().zip(sniffed) {
                let mapping = names
                    .iter()
                    .map(|column| file.names.iter().position(|n| n.eq_ignore_ascii_case(column)))
                    .collect();
                files.push(CsvFile {
                    path: path.clone(),
                    columns: file.names.len(),
                    dialect: file.dialect,
                    header: file.header,
                    skip: file.skip,
                    rows_until_header: file.rows_until_header,
                    mapping,
                });
            }
        } else {
            if arguments.columns.is_none() {
                Self::merge_sniffed_schemas(&arguments, &first, &mut types)?;
            }
            replace_null_with_varchar(&mut types);
            for path in &arguments.files {
                files.push(CsvFile {
                    path: path.clone(),
                    dialect: first.dialect.clone(),
                    header: first.header,
                    skip: first.skip,
                    rows_until_header: first.rows_until_header,
                    columns: types.len(),
                    mapping: (0..types.len()).map(Some).collect(),
                });
            }
        }
        let mut output_names = names.clone();
        let mut output_types = types.clone();
        if let Some(column) = &arguments.filename {
            output_names.push(column.clone());
            output_types.push(LogicalType::VARCHAR);
        }
        let hive = hive_columns_with(&arguments.files, &arguments.hive, &names)?;
        for (key, ty) in &hive {
            output_names.push(key.clone());
            output_types.push(ty.clone());
        }
        let alias = if table_ref.alias.is_empty() {
            name.clone()
        } else {
            table_ref.alias.clone()
        };
        let output_names = Self::alias_columns(table_ref, &alias, output_names)?;
        let table_index = self.generate_table_index();
        let (virtual_columns, bound_virtual) = crate::execution::multi_file::bind_virtual_columns(&output_names);
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            table_index,
            names: output_names,
            types: output_types,
            row_id: None,
            virtual_columns: bound_virtual,
            generated: Vec::new(),
        });
        let wanted = &arguments.options.force_not_null;
        for column in wanted {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(column)) {
                return Err(binder_error(format!(
                    "\"force_not_null\" expected to find {column}, but it was not found in the table"
                )));
            }
        }
        let force_not_null: Vec<bool> = names
            .iter()
            .map(|name| wanted.iter().any(|n| n.eq_ignore_ascii_case(name)))
            .collect();
        let manual_types: Vec<bool> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                arguments.columns.is_some()
                    || i < arguments.types_by_position.len()
                    || arguments
                        .types_by_name
                        .iter()
                        .any(|(n, _)| n.eq_ignore_ascii_case(name))
            })
            .collect();
        let rejects = if arguments.storing {
            Some(self.csv_rejects(&arguments)?)
        } else {
            None
        };
        let scan = CsvScanData {
            files,
            rejects,
            hive,
            force_not_null,
            manual_types,
            columns_set: arguments.columns.is_some(),
            mismatch: first_mismatch,
            union_by_name: arguments.union_by_name,
            parsing: Parsing {
                date_format: first_formats.0,
                timestamp_format: first_formats.1,
                decimal_separator: arguments.options.decimal_separator,
                thousands: arguments.options.thousands,
            },
            options: arguments.options,
            names,
            types,
            filename: arguments.filename.is_some(),
            first_data: std::sync::Mutex::new(Some(first_data)),
            virtual_columns,
        };
        Ok(LogicalOperator::new(
            LogicalOperatorKind::CsvScan {
                table_index,
                scan: Arc::new(scan),
            },
            vec![],
        ))
    }

    /// `allow_empty` and no file: one column, and nothing in it.
    fn bind_empty_csv(&mut self, table_ref: &TableRef, name: &str, arguments: CsvArguments) -> Result<LogicalOperator> {
        let (names, types) = match arguments.columns {
            Some(columns) => columns.into_iter().unzip(),
            None => (vec!["empty".to_string()], vec![LogicalType::BOOLEAN]),
        };
        let alias = if table_ref.alias.is_empty() {
            name.to_string()
        } else {
            table_ref.alias.clone()
        };
        let output_names = Self::alias_columns(table_ref, &alias, names.clone())?;
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            table_index,
            names: output_names,
            types: types.clone(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let scan = CsvScanData {
            files: Vec::new(),
            options: arguments.options,
            names,
            types,
            parsing: Parsing::default(),
            hive: Vec::new(),
            force_not_null: Vec::new(),
            manual_types: Vec::new(),
            columns_set: false,
            mismatch: String::new(),
            union_by_name: false,
            filename: false,
            rejects: None,
            first_data: std::sync::Mutex::new(None),
            virtual_columns: Vec::new(),
        };
        Ok(LogicalOperator::new(
            LogicalOperatorKind::CsvScan {
                table_index,
                scan: Arc::new(scan),
            },
            vec![],
        ))
    }

    /// `CSVRejectsTable::GetOrCreate` and `InitializeTable`: the temporary tables of rejects.
    fn csv_rejects(&self, arguments: &CsvArguments) -> Result<CsvRejects> {
        static SCANS: AtomicU64 = AtomicU64::new(0);
        let scans_name = arguments.rejects_scan.as_deref().unwrap_or("reject_scans");
        let errors_name = arguments.rejects_table.as_deref().unwrap_or("reject_errors");
        if scans_name.eq_ignore_ascii_case(errors_name) {
            return Err(binder_error(
                "The names of the rejects scan and rejects error tables can't be the same. Use different names for \
                 these tables.",
            ));
        }
        let temp = self
            .catalog
            .session_temp()
            .ok_or_else(|| crate::unported("store_rejects outside of a connection"))?;
        let table = |name: &str| match temp.get_entry(name) {
            Some(TableOrView::Table(table)) => Some(table),
            _ => None,
        };
        let ours = |entry: &Option<Arc<TableCatalogEntry>>, column: &str| {
            entry.as_ref().is_none_or(|table| table.column_index(column).is_some())
        };
        let (scans, errors) = (table(scans_name), table(errors_name));
        let (scans_ok, errors_ok) = (ours(&scans, "file_path"), ours(&errors, "error_message"));
        if !scans_ok || !errors_ok {
            let mut message = String::new();
            if scans.is_some() {
                message += &format!("Reject Scan Table name \"{scans_name}\" is already in use. ");
            }
            if errors.is_some() {
                message += &format!("Reject Error Table name \"{errors_name}\" is already in use. ");
            }
            message += "Either drop the used name(s), or give other name options in the CSV Reader function.\n";
            return Err(binder_error(message));
        }
        let column = ColumnDefinition::new;
        let (big, text) = (LogicalType::UBIGINT, LogicalType::VARCHAR);
        let error_type = enum_type_from(
            [
                "CAST",
                "MISSING COLUMNS",
                "TOO MANY COLUMNS",
                "UNQUOTED VALUE",
                "LINE SIZE OVER MAXIMUM",
                "INVALID ENCODING",
                "INVALID STATE",
            ]
            .map(str::to_string)
            .to_vec(),
        )?;
        let create = |name: &str, columns: Vec<ColumnDefinition>| -> Result<Arc<TableCatalogEntry>> {
            temp.create_table(name, columns, Vec::new(), Vec::new(), OnCreateConflict::Ignore)?;
            table(name).ok_or_else(|| binder_error("The rejects tables could not be created"))
        };
        let scans = create(
            scans_name,
            vec![
                column("scan_id".to_string(), big.clone()),
                column("file_id".to_string(), big.clone()),
                column("file_path".to_string(), text.clone()),
                column("delimiter".to_string(), text.clone()),
                column("quote".to_string(), text.clone()),
                column("escape".to_string(), text.clone()),
                column("newline_delimiter".to_string(), text.clone()),
                column("skip_rows".to_string(), LogicalType::UINTEGER),
                column("has_header".to_string(), LogicalType::BOOLEAN),
                column("columns".to_string(), text.clone()),
                column("date_format".to_string(), text.clone()),
                column("timestamp_format".to_string(), text.clone()),
                column("user_arguments".to_string(), text.clone()),
            ],
        )?;
        let errors = create(
            errors_name,
            vec![
                column("scan_id".to_string(), big.clone()),
                column("file_id".to_string(), big.clone()),
                column("line".to_string(), big.clone()),
                column("line_byte_position".to_string(), big.clone()),
                column("byte_position".to_string(), big.clone()),
                column("column_idx".to_string(), big),
                column("column_name".to_string(), text.clone()),
                column("error_type".to_string(), error_type),
                column("csv_line".to_string(), text.clone()),
                column("error_message".to_string(), text),
            ],
        )?;
        let user: Vec<String> = arguments
            .user_arguments
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        Ok(CsvRejects {
            errors,
            scans,
            limit: arguments.rejects_limit,
            line_size_limit: arguments.rejects_line_size_limit,
            scan_id: SCANS.fetch_add(1, Ordering::Relaxed),
            user_arguments: (!user.is_empty()).then(|| user.join(", ")),
        })
    }

    /// `CSVSniffFunction`, as one row of constants.
    pub(super) fn bind_sniff_csv(&mut self, call: &FunctionExpr, table_ref: &TableRef) -> Result<LogicalOperator> {
        if call.args.first().is_some_and(
            |argument| matches!(self.constant_argument(&named_argument(argument).expr), Ok(v) if v.is_null()),
        ) {
            return Err(binder_error("sniff_csv cannot take NULL as a file path parameter"));
        }
        let arguments = self.csv_arguments("sniff_csv", call)?;
        if arguments.auto_detect == Some(false) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "sniff_csv function does not accept auto_detect variable set to false",
            ));
        }
        let path = &arguments.files[0];
        let (mut sniffed, _) = Self::csv_sniff(&arguments, path)?;
        replace_null_with_varchar(&mut sniffed.types);
        let dialect = &sniffed.dialect;
        let new_line = new_line_identifier(&sniffed.dialect);
        let shown = |byte: Option<u8>| byte.map_or("(empty)".to_string(), |b| char::from(b).to_string());
        let quoted = |byte: Option<u8>| byte.map_or(String::new(), |b| char::from(b).to_string().replace('\'', "''"));
        let delimiter = String::from_utf8_lossy(&dialect.delimiter).into_owned();
        let structs: Vec<String> = sniffed
            .names
            .iter()
            .zip(&sniffed.types)
            .map(|(name, ty)| {
                format!(
                    "{{'name': {}, 'type': {}}}",
                    string_literal(name),
                    string_literal(&ty.to_string())
                )
            })
            .collect();
        let columns: Vec<String> = sniffed
            .names
            .iter()
            .zip(&sniffed.types)
            .map(|(name, ty)| format!("{}: {}", string_literal(name), string_literal(&ty.to_string())))
            .collect();
        let has_date = sniffed.types.iter().any(|ty| ty.id() == LogicalTypeId::Date);
        let has_timestamp = sniffed.types.iter().any(|ty| ty.id() == LogicalTypeId::Timestamp);
        let date_format = has_date.then(|| sniffed.date_format.clone().unwrap_or_else(|| "%Y-%m-%d".to_string()));
        let timestamp_format = if has_timestamp {
            sniffed.timestamp_format.clone()
        } else {
            None
        };
        let given = |names: &[&str]| {
            arguments
                .user_arguments
                .iter()
                .any(|(n, _)| names.contains(&n.as_str()))
        };
        let mut prompt = format!("FROM read_csv({}, auto_detect=false, ", string_literal(path));
        if !given(&["delim", "sep", "delimiter"]) {
            prompt += &format!("delim='{}', ", delimiter.replace('\'', "''"));
        }
        if !given(&["quote"]) {
            prompt += &format!("quote='{}', ", quoted(dialect.quote));
        }
        if !given(&["escape"]) {
            prompt += &format!("escape='{}', ", quoted(dialect.escape));
        }
        if !given(&["new_line"]) {
            prompt += &format!("new_line='{new_line}', ");
        }
        if !given(&["skip"]) {
            prompt += &format!("skip={}, ", sniffed.skip);
        }
        if !given(&["comment"]) {
            prompt += &format!("comment='{}', ", quoted(dialect.comment));
        }
        if !given(&["header"]) {
            prompt += &format!("header={}, ", sniffed.header);
        }
        prompt += &format!("columns={{{}}}", columns.join(", "));
        if let Some(format) = &date_format
            && !given(&["dateformat", "date_format"])
        {
            prompt += &format!(", dateformat='{format}'");
        }
        if let Some(format) = &timestamp_format
            && !given(&["timestampformat", "timestamp_format"])
        {
            prompt += &format!(", timestampformat='{format}'");
        }
        let user: Vec<String> = arguments
            .user_arguments
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        if !user.is_empty() {
            prompt += &format!(", {}", user.join(", "));
        }
        prompt += ");";
        let optional = |text: Option<String>| text.map_or("NULL::VARCHAR".to_string(), |t| string_literal(&t));
        let sql = format!(
            "SELECT {} AS \"Delimiter\", {} AS \"Quote\", {} AS \"Escape\", {} AS \"NewLineDelimiter\", \
             {} AS \"Comment\", {}::UBIGINT AS \"SkipRows\", {} AS \"HasHeader\", [{}] AS \"Columns\", \
             {} AS \"DateFormat\", {} AS \"TimestampFormat\", {} AS \"UserArguments\", {} AS \"Prompt\"",
            string_literal(&delimiter),
            string_literal(&shown(dialect.quote)),
            string_literal(&shown(dialect.escape)),
            string_literal(new_line),
            string_literal(&shown(dialect.comment)),
            sniffed.skip,
            sniffed.header,
            structs.join(", "),
            optional(date_format.clone()),
            optional(timestamp_format.clone()),
            optional((!user.is_empty()).then(|| user.join(", "))),
            string_literal(&prompt),
        );
        let select = parse_select(&sql, "Expected a single SELECT statement")?;
        self.bind_rewritten(&select, table_ref, "sniff_csv")
    }

    /// `DatabaseManager::IsAttachedDatabaseFile`: `path` or one of its WAL files is a database file.
    fn is_attached_database_file(&self, path: &str) -> bool {
        if path.contains("://") {
            return false;
        }
        let Some(session) = &self.session else {
            return false;
        };
        let files = (session.database_files)();
        let given = std::path::Path::new(path);
        let canonical = std::fs::canonicalize(given).unwrap_or_else(|_| {
            // a file that does not exist yet: its directory canonicalized
            let parent = given
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            match (std::fs::canonicalize(parent), given.file_name()) {
                (Ok(directory), Some(name)) => directory.join(name),
                _ => given.to_path_buf(),
            }
        });
        let canonical = canonical.to_string_lossy();
        [".wal", ".wal.checkpoint", ".wal.recovery", ""]
            .into_iter()
            .any(|suffix| {
                canonical
                    .strip_suffix(suffix)
                    .is_some_and(|database| files.iter().any(|file| file.to_string_lossy() == database))
            })
    }

    /// The file a COPY names, with its format.
    fn copy_target(&mut self, info: &CopyInfo) -> Result<(String, bool)> {
        let path = match &info.file_path_expression {
            Some(expression) => {
                let value = self.constant_argument(expression)?;
                if value.logical_type().id() != LogicalTypeId::Varchar {
                    return Err(binder_error(
                        "Unsupported parameter type for filename: expected e.g. TARGET 'file.parquet'",
                    ));
                }
                if value.is_null() {
                    return Err(binder_error(
                        "NULL is not supported as a valid option for COPY option filename",
                    ));
                }
                text(&value)?
            }
            None => info.file_path.clone(),
        };
        // `FileSystem::ExpandPath`: `~` and `file:` URLs
        let path = crate::execution::file_system::expand_path(&path);
        if !info.is_from && !path.is_empty() && self.is_attached_database_file(&path) {
            return Err(binder_error(format!(
                "Cannot COPY to \"{path}\": the file belongs to an attached database"
            )));
        }
        let format = if info.format.is_empty() {
            let lower = path.to_lowercase();
            ["parquet", "json", "ndjson", "jsonl"]
                .into_iter()
                .find(|extension| lower.ends_with(&format!(".{extension}")))
                .unwrap_or("csv")
                .to_string()
        } else {
            info.format.to_lowercase()
        };
        let known = matches!(
            format.as_str(),
            "csv" | "tsv" | "auto" | "json" | "ndjson" | "jsonl" | "blob"
        );
        // a format taken from the file's extension that no COPY function has is CSV
        let format = if !known && !info.format_given {
            "csv".to_string()
        } else {
            format
        };
        if format == "blob" {
            if info.is_from {
                return Err(binder_error("COPY FROM is not supported for FORMAT BLOB"));
            }
            self.copy_json = false;
            return Ok((path, false));
        }
        if !matches!(format.as_str(), "csv" | "tsv" | "auto" | "json" | "ndjson" | "jsonl") {
            return Err(crate::unported(format!("COPY with the format {format}")));
        }
        self.copy_json = matches!(format.as_str(), "json" | "ndjson" | "jsonl");
        Ok((path, format == "tsv"))
    }

    /// `CopyOptionMode`, and that options of text are text.
    fn check_copy_options(options: &[(String, Value)], reading: bool) -> Result<()> {
        const READ_ONLY: [&str; 18] = [
            "auto_detect",
            "sample_size",
            "skip",
            "max_line_size",
            "maximum_line_size",
            "ignore_errors",
            "buffer_size",
            "decimal_separator",
            "null_padding",
            "parallel",
            "allow_quoted_nulls",
            "store_rejects",
            "force_not_null",
            "rejects_table",
            "rejects_scan",
            "rejects_limit",
            "encoding",
            "thousands",
        ];
        const WRITE_ONLY: [&str; 3] = ["force_quote", "prefix", "suffix"];
        for (name, value) in options {
            if !reading && READ_ONLY.contains(&name.as_str()) {
                return Err(binder_error(format!(
                    "Option \"{name}\" is not supported for writing - only for reading"
                )));
            }
            if reading && WRITE_ONLY.contains(&name.as_str()) {
                return Err(binder_error(format!(
                    "Option \"{name}\" is not supported for reading - only for writing"
                )));
            }
            if matches!(
                name.as_str(),
                "delim" | "delimiter" | "sep" | "separator" | "quote" | "escape" | "comment" | "new_line" | "encoding"
            ) && !matches!(
                value.logical_type().id(),
                LogicalTypeId::Varchar | LogicalTypeId::SqlNull
            ) {
                // an option without a value is `true`
                if value.logical_type().id() == LogicalTypeId::Boolean {
                    return Err(binder_error(format!("\"{name}\" requires an argument of type VARCHAR")));
                }
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Copy option \"{name}\" expected an argument of type VARCHAR - the argument \"{}\" of type {} \
                         could not be cast as this type",
                        value.try_to_string().unwrap_or_default(),
                        value.logical_type()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The options of a COPY, as `(name, value)`; a flag without a value is true.
    pub(super) fn copy_options(&mut self, info: &CopyInfo) -> Result<Vec<(String, Value)>> {
        let mut result = Vec::new();
        for (name, expression) in &info.parsed_options {
            if name.eq_ignore_ascii_case("partition_path") {
                continue;
            }
            let value = match expression {
                Some(expression) if expression.is_empty_star() => Value::varchar("*"),
                Some(expression) => self.constant_argument(expression)?,
                None => Value::boolean(true),
            };
            // `BindCopyOption`; SHREDDING says itself what it takes
            // JSON's COPY says itself which of its options cannot be NULL (`ThrowJSONCopyNullException`)
            if value.is_null() && !name.eq_ignore_ascii_case("shredding") && !self.copy_json {
                return Err(binder_error(format!(
                    "NULL is not supported as a valid option for COPY option \"{name}\""
                )));
            }
            result.push((name.to_lowercase(), value));
        }
        for (name, literals) in &info.options {
            let value = match literals.as_slice() {
                [] => Value::boolean(true),
                [literal] => self.constant_argument(&Expr::constant(literal.clone()))?,
                _ => return Err(crate::unported(format!("the COPY option {name} with several values"))),
            };
            // JSON's COPY says itself which of its options cannot be NULL (`ThrowJSONCopyNullException`)
            if value.is_null() && !name.eq_ignore_ascii_case("shredding") && !self.copy_json {
                return Err(binder_error(format!(
                    "NULL is not supported as a valid option for COPY option \"{name}\""
                )));
            }
            result.push((name.to_lowercase(), value));
        }
        Ok(result)
    }

    pub(super) fn bind_copy(&mut self, info: &CopyInfo) -> Result<BoundStatement> {
        // `ParameterNotResolvedException`: a COPY with parameters in its target or options binds
        // when it is executed; every parameter is seen first
        let preparing = self.parameters.as_ref().is_some_and(|p| p.values.is_none());
        let has_parameter = |e: &Expr| super::macros::has_parameter(e);
        if preparing
            && (info.file_path_expression.as_deref().is_some_and(has_parameter)
                || info
                    .parsed_options
                    .iter()
                    .any(|(_, e)| e.as_ref().is_some_and(has_parameter)))
        {
            if let Some(select) = &info.select {
                let mut binder = self.child_binder();
                binder.bind_query_node(select)?;
                self.next_table_index = binder.next_table_index;
            }
            // binding them sees their parameters; their values come later
            if let Some(expression) = &info.file_path_expression {
                let _ = self.constant_argument(expression);
            }
            for expression in info.parsed_options.iter().filter_map(|(_, e)| e.as_ref()) {
                let _ = self.constant_argument(expression);
            }
            return Err(Error::new(
                ErrorKind::ParameterNotResolved,
                "Parameter types could not be resolved",
            ));
        }
        if info.is_from
            && info
                .parsed_options
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("partition_path"))
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Option partition_path is not supported for reading - only for writing",
            ));
        }
        if let Some(path) = self.parquet_copy_path(info)? {
            self.check_file_access(&path)?;
            if !info.is_from {
                verify_not_extension(&path)?;
            }
            return if info.is_from {
                self.bind_copy_from_parquet(info, &path)
            } else {
                self.bind_copy_to_parquet(info, path)
            };
        }
        let (path, tsv) = self.copy_target(info)?;
        self.check_file_access(&path)?;
        if !info.is_from {
            verify_not_extension(&path)?;
        }
        if !info.is_from && info.format.eq_ignore_ascii_case("blob") {
            return self.bind_copy_to_blob(info, path);
        }
        if info.is_from {
            self.bind_copy_from(info, &path, tsv)
        } else {
            self.bind_copy_to(info, path, tsv)
        }
    }

    /// `Binder::BindCopyFrom`.
    fn bind_copy_from(&mut self, info: &CopyInfo, path: &str, tsv: bool) -> Result<BoundStatement> {
        let table = self
            .catalog
            .get_table(self.schema_of(&info.table)?.as_deref(), &info.table.name)?;
        let columns: Vec<String> = if info.select_list.is_empty() {
            table.columns.iter().map(|c| c.name.clone()).collect()
        } else {
            info.select_list.clone()
        };
        let mut fields = Vec::with_capacity(columns.len());
        for column in &columns {
            let Some(index) = table.column_index(column) else {
                return Err(binder_error(format!(
                    "Table \"{}\" does not have a column with name \"{column}\"",
                    table.name
                )));
            };
            let ty = &table.columns[index].ty;
            fields.push(format!(
                "{}: '{}'",
                string_literal(column),
                ty.to_string().replace('\'', "''")
            ));
        }
        let mut arguments = vec![string_literal(path), format!("columns := {{{}}}", fields.join(", "))];
        let options = self.copy_options(info)?;
        Self::check_copy_options(&options, true)?;
        if !options.iter().any(|(name, _)| name == "auto_detect") {
            arguments.push("auto_detect := true".to_string());
        }
        let delimiter_given = options
            .iter()
            .any(|(name, _)| matches!(name.as_str(), "delim" | "sep" | "delimiter"));
        if tsv && !delimiter_given {
            arguments.push("delimiter := '\t'".to_string());
        }
        // `MultiFileBindCopy`: a COPY FROM only reads hive partitions when told to
        if !options.iter().any(|(name, _)| name.starts_with("hive_")) {
            arguments.push("hive_partitioning := false".to_string());
        }
        for (name, value) in options {
            if name == "format" {
                continue;
            }
            arguments.push(format!("{} := {}", sql_identifier(&name), value_literal(&value)?));
        }
        let reader = if self.copy_json { "read_json" } else { "read_csv" };
        if self.copy_json {
            arguments.retain(|argument| !argument.starts_with("auto_detect"));
        }
        let select = parse_select(
            &format!("SELECT * FROM {reader}({})", arguments.join(", ")),
            "Expected a single SELECT statement",
        )?;
        self.bind_insert(&InsertStatement {
            cte_map: Default::default(),
            table: info.table.clone(),
            table_alias: None,
            by_name: false,
            columns,
            values: InsertValues::Select(Box::new(select)),
            on_conflict: None,
            returning: Vec::new(),
        })
    }

    /// The query of a COPY TO, sorted by its `ORDER BY` option (`ParseOrderByColumns`), with the checks of
    /// `ValidateCopyToOptionCombinations` on the options that shape the files.
    pub(super) fn copy_to_select(&mut self, info: &CopyInfo) -> Result<SelectStatement> {
        let select = match &info.select {
            Some(query) => SelectStatement {
                node: (**query).clone(),
            },
            None => {
                let columns = if info.select_list.is_empty() {
                    "*".to_string()
                } else {
                    let names: Vec<_> = info.select_list.iter().map(|c| sql_identifier(c)).collect();
                    names.join(", ")
                };
                parse_select(&format!("SELECT {columns} FROM {}", info.table), "Expected a table")?
            }
        };
        let options = self.copy_options(info)?;
        let given = |name: &str| options.iter().find(|(n, _)| n == name).map(|(_, v)| v);
        let flag = |name: &str| -> Result<bool> {
            match given(name) {
                Some(value) => is_true(value, name),
                None => Ok(false),
            }
        };
        let partitioned = given("partition_by").is_some() || given("partitioned_by").is_some();
        let order_by = given("order_by");
        if given("use_tmp_file").is_some() {
            if flag("per_thread_output")? {
                return Err(Error::not_implemented(
                    "Can't combine USE_TMP_FILE and PER_THREAD_OUTPUT for COPY",
                ));
            }
            if partitioned {
                return Err(Error::not_implemented(
                    "Can't combine USE_TMP_FILE and PARTITIONED BY for COPY",
                ));
            }
            if order_by.is_some() {
                return Err(Error::not_implemented(
                    "Can't combine USE_TMP_FILE and ORDER BY for COPY",
                ));
            }
            let rotates = ["file_size_bytes", "row_groups_per_file", "batches_per_file"]
                .iter()
                .any(|name| given(name).is_some());
            if rotates {
                return Err(Error::not_implemented(
                    "Can't combine USE_TMP_FILE and FILE_SIZE_BYTES/BATCHES_PER_FILE for COPY",
                ));
            }
        }
        // `CreatePlan(LogicalCopyToFile)`: these write files in whatever order rows come
        let splits = flag("per_thread_output")?
            || partitioned
            || order_by.is_some()
            || ["file_size_bytes", "row_groups_per_file", "batches_per_file"]
                .iter()
                .any(|name| given(name).is_some());
        if splits && flag("preserve_order")? {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "PRESERVE_ORDER is not supported with these parameters",
            ));
        }
        if flag("per_thread_output")? && partitioned {
            return Err(Error::not_implemented(
                "Can't combine PER_THREAD_OUTPUT and PARTITIONED BY for COPY",
            ));
        }
        if order_by.is_none() {
            return Ok(select);
        }
        if flag("per_thread_output")? {
            return Err(Error::not_implemented(
                "Can't combine PER_THREAD_OUTPUT and ORDER BY for COPY",
            ));
        }
        if given("write_empty_file").is_some_and(|value| !is_true(value, "write_empty_file").unwrap_or(true)) {
            return Err(Error::not_implemented(
                "Can't combine WRITE_EMPTY_FILE false with ORDER BY",
            ));
        }
        // the rows are sorted by run as they are written (`bind_copy_order`)
        Ok(select)
    }

    /// `ParseOrderByColumns`: the ORDER_BY of a COPY TO, bound against the columns of its query.
    pub(super) fn bind_copy_order(
        &mut self,
        info: &CopyInfo,
        names: &[String],
        types: &[LogicalType],
    ) -> Result<
        Vec<(
            crate::planner::expression::BoundExpr,
            duckrs_common::sort_key::OrderModifiers,
        )>,
    > {
        use crate::planner::column_binding_resolver::resolve_expression;
        use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
        use duckrs_parser::ast::{ExprKind, ResultModifier};
        let options = self.copy_options(info)?;
        let Some((_, value)) = options.iter().find(|(name, _)| name == "order_by") else {
            return Ok(Vec::new());
        };
        let clause = strings(value)?.join(", ");
        let select = parse_select(
            &format!("SELECT 1 ORDER BY {clause}"),
            "Expected a single SELECT statement",
        )?;
        let orders = select
            .node
            .modifiers
            .iter()
            .find_map(|modifier| match modifier {
                ResultModifier::Order(orders) => Some(orders.clone()),
                _ => None,
            })
            .unwrap_or_default();
        const SUBQUERY: &str = "subqueries are not allowed in ORDER_BY";
        for order in &orders {
            let mut subquery = false;
            duckrs_parser::transformer::visit_expr_mut(&mut order.expr.clone(), &mut |expr: &mut Expr| {
                subquery |= matches!(expr.kind, ExprKind::Subquery { .. });
            });
            if subquery {
                return Err(binder_error(SUBQUERY));
            }
        }
        let mut binder = Binder::new(self.catalog);
        binder.options = self.options.clone();
        binder.session = self.session.clone();
        binder.parameters = self.parameters.clone();
        binder.aggregate_error = "aggregate functions are not allowed in ORDER_BY";
        let table_index = binder.generate_table_index();
        binder.bind_context.add_binding(Binding {
            alias: "__copy_input".to_string(),
            schema: None,
            table_index,
            names: names.to_vec(),
            types: types.to_vec(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let bindings: Vec<ColumnBinding> = (0..names.len())
            .map(|column_index| ColumnBinding {
                table_index,
                column_index,
            })
            .collect();
        fn has_subquery(expr: &BoundExpr) -> bool {
            matches!(expr.kind, BoundExprKind::Subquery { .. }) || expr.children().into_iter().any(has_subquery)
        }
        let mut result = Vec::with_capacity(orders.len());
        for order in orders {
            let bound = binder.bind_expression(&order.expr)?;
            if has_subquery(&bound) {
                return Err(binder_error(SUBQUERY));
            }
            // the rows sort as the collation of the term says
            let ty = bound.return_type.clone();
            let (mut bound, _) = crate::planner::collation::push_collation(
                bound,
                &ty,
                &self.default_collation(),
                crate::planner::collation::CollationType::AllCollations,
            )?;
            resolve_expression(&mut bound, &bindings)?;
            result.push((bound, self.options.resolve_order(order.order, order.nulls)));
        }
        Ok(result)
    }

    /// `Binder::BindCopyTo`.
    /// `WriteBlobBind`: the bytes of a single BLOB column, one value after the other.
    fn bind_copy_to_blob(&mut self, info: &CopyInfo, path: String) -> Result<BoundStatement> {
        let select = self.copy_to_select(info)?;
        let bound = self.bind_select_statement(&select)?;
        if bound.types.len() != 1 || bound.types[0].id() != LogicalTypeId::Blob {
            return Err(binder_error(
                "\"COPY (FORMAT BLOB)\" only supports a single BLOB column",
            ));
        }
        let mut compression = Compression::Auto;
        let valueless = info
            .options
            .iter()
            .filter(|(_, values)| values.is_empty())
            .map(|(name, _)| name.to_lowercase())
            .chain(
                info.parsed_options
                    .iter()
                    .filter(|(_, e)| e.is_none())
                    .map(|(n, _)| n.to_lowercase()),
            )
            .collect::<Vec<_>>();
        for (name, value) in self.copy_options(info)? {
            if name == "compression" && valueless.contains(&name) {
                return Err(binder_error(format!(
                    "COPY (FORMAT BLOB) option {} requires a value",
                    sql_identifier(&name)
                )));
            }
            match name.as_str() {
                "format" => {}
                "compression" => {
                    // `ParseStringOption`
                    let mut value = value;
                    if let ValueData::List(items) = value.data() {
                        if items.len() != 1 {
                            return Err(binder_error(format!(
                                "{name} expects a single argument as a string value"
                            )));
                        }
                        value = items[0].clone();
                    }
                    if value.logical_type().id() != LogicalTypeId::Varchar {
                        return Err(binder_error(format!("{name} expects a string argument!")));
                    }
                    compression = Compression::parse(&text(&value)?)?;
                }
                _ => {
                    return Err(binder_error(format!(
                        "Unrecognized option for COPY (FORMAT BLOB): {}",
                        sql_identifier(&name)
                    )));
                }
            }
        }
        let compression = compression.for_path(&path);
        let options = CsvWriteOptions {
            raw_blob: true,
            header: false,
            new_line: String::new(),
            ..Default::default()
        };
        let (result_names, return_type) = copy_result(&options.output);
        let result_types = options.output.types();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::CopyToFile {
                path,
                names: bound.names.clone(),
                options,
                compression,
            },
            vec![bound.plan],
        );
        Ok(BoundStatement {
            plan,
            names: result_names,
            types: result_types,
            column_aliases: Vec::new(),
            return_type,
        })
    }

    fn bind_copy_to(&mut self, info: &CopyInfo, path: String, tsv: bool) -> Result<BoundStatement> {
        let select = self.copy_to_select(info)?;
        let mut options = CsvWriteOptions::default();
        if tsv {
            options.delimiter = b"\t".to_vec();
        }
        let mut escape_given = false;
        let mut header_given = false;
        let mut force_quote = Vec::new();
        let mut compression = Compression::Auto;
        let bound = self.bind_select_statement(&select)?;
        let copy_options = self.copy_options(info)?;
        if self.copy_json {
            // `GetSingleJSONCopyValue` and `GetSingleJSONCopyString`
            for (name, value) in &copy_options {
                if value.is_null() {
                    return Err(binder_error(format!(
                        "COPY (FORMAT JSON) parameter {name} cannot be NULL."
                    )));
                }
                let text_option = matches!(
                    name.as_str(),
                    "dateformat"
                        | "date_format"
                        | "timestampformat"
                        | "timestamp_format"
                        | "file_extension"
                        | "filename_pattern"
                        | "encoding"
                        | "compression"
                );
                if text_option && value.logical_type().id() != LogicalTypeId::Varchar {
                    return Err(binder_error(format!(
                        "COPY (FORMAT JSON) parameter {name} expects a VARCHAR argument, but got {}.",
                        value.logical_type()
                    )));
                }
            }
        }
        Self::check_copy_options(&copy_options, false)?;
        if self.copy_json {
            options.json_names = Some(unique_names(&bound.names));
            options.header = false;
            options.new_line = "\n".to_string();
            options.suffix = "\n".to_string();
        }
        let mut partition = PartitionOptions::default();
        let (mut file_size_bytes, mut per_thread) = (None, false);
        let mut output = OutputOptionsBuilder::default();
        let ordered = info
            .parsed_options
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("order_by"));
        for (name, value) in copy_options {
            if partition.take(&name, &value)? || output.take(&name, &value)? {
                continue;
            }
            match name.as_str() {
                "file_size_bytes" => {
                    file_size_bytes = Some(parse_bytes_argument(&name, &value)?);
                    continue;
                }
                "per_thread_output" => {
                    per_thread = is_true(&value, &name)?;
                    continue;
                }
                "row_groups_per_file" | "batches_per_file" => {
                    return Err(Error::not_implemented(format!(
                        "Can't use file rotation (e.g., ROW_GROUPS_PER_FILE) with FORMAT {}",
                        if self.copy_json { "json" } else { "csv" }
                    )));
                }
                _ => {}
            }
            match name.as_str() {
                "format" | "order_by" => {}
                "array" if self.copy_json => {
                    if is_true(&value, &name)? {
                        options.prefix = "[\n\t".to_string();
                        options.new_line = ",\n\t".to_string();
                        options.suffix = "\n]\n".to_string();
                    }
                }
                "dateformat" | "date_format" => options.date_format = Some(text(&value)?),
                "timestampformat" | "timestamp_format" => options.timestamp_format = Some(text(&value)?),
                "delim" | "sep" | "delimiter" => options.delimiter = delimiter(&value)?,
                "quote" => options.quote = single_byte(&value, "quote")?,
                "escape" => {
                    options.escape = single_byte(&value, "escape")?;
                    escape_given = true;
                }
                "comment" => options.comment = single_byte(&value, "comment")?,
                "header" => {
                    options.header = is_true(&value, &name)?;
                    header_given = options.header;
                }
                "null" | "nullstr" => options.null_str = text(&value)?,
                "new_line" => options.new_line = new_line(&text(&value)?)?,
                "prefix" => options.prefix = text(&value)?,
                "suffix" => options.suffix = text(&value)?,
                "force_quote" => force_quote = quoted_columns(&value, &bound.names)?,
                "compression" => compression = Compression::parse(&text(&value)?)?,
                // the file ends up the same, unless a write fails half way
                "use_tmp_file" => {}
                _ if !is_copy_option(CSV_COPY_OPTIONS, &name) => {
                    return Err(unrecognized_copy_option(&name, "csv", CSV_COPY_OPTIONS));
                }
                _ => return Err(crate::unported(format!("the COPY option {name}"))),
            }
        }
        if !escape_given {
            options.escape = options.quote;
        }
        if !self.copy_json && options.prefix.is_empty() != options.suffix.is_empty() {
            return Err(binder_error(
                "COPY ... (FORMAT CSV) must have both PREFIX and SUFFIX, or none at all",
            ));
        }
        if !options.prefix.is_empty() && header_given {
            return Err(binder_error(
                "COPY ... (FORMAT CSV)'s HEADER cannot be combined with PREFIX/SUFFIX",
            ));
        }
        options.force_quote = force_quote;
        let compression = compression.for_path(&path);
        let mut extension = if self.copy_json { "json" } else { "csv" }.to_string();
        match compression {
            Compression::Gzip => extension += ".gz",
            Compression::Zstd => extension += ".zst",
            _ => {}
        }
        let order = self.bind_copy_order(info, &bound.names, &bound.types)?;
        options.partitioning = partition.bind_ordered(&path, &bound.names, &extension, !order.is_empty())?;
        if let Some(partitioning) = options.partitioning.as_mut() {
            partitioning.order = order;
        }
        self.bind_partition_path(
            info,
            &bound.names,
            &bound.types,
            &partition,
            options.partitioning.as_mut(),
        )?;
        self.set_max_open_files(options.partitioning.as_mut());
        options.rotation = partition.rotation(&path, &extension, file_size_bytes, None, per_thread)?;
        let format = if self.copy_json { "json" } else { "csv" };
        options.output = output.build(options.partitioning.is_some(), per_thread, ordered, Some(format))?;
        let (result_names, return_type) = copy_result(&options.output);
        let result_types = options.output.types();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::CopyToFile {
                path,
                names: unique_names(&bound.names),
                options,
                compression,
            },
            vec![bound.plan],
        );
        Ok(BoundStatement {
            plan,
            names: result_names,
            types: result_types,
            column_aliases: Vec::new(),
            return_type,
        })
    }
}

/// The options of a COPY TO that write a directory of hive partitions (`PARTITION_BY` and friends).
pub(super) struct PartitionOptions {
    columns: Option<Vec<String>>,
    write_columns: bool,
    overwrite_or_ignore: bool,
    overwrite: bool,
    append: bool,
    /// whether OVERWRITE_OR_IGNORE, OVERWRITE or APPEND came
    overwrite_mode_given: bool,
    hive_file_pattern: bool,
    hive_file_pattern_given: bool,
    filename_pattern: Option<String>,
    extension: Option<String>,
}

impl Default for PartitionOptions {
    fn default() -> Self {
        Self {
            columns: None,
            write_columns: false,
            overwrite_or_ignore: false,
            overwrite: false,
            append: false,
            overwrite_mode_given: false,
            hive_file_pattern: true,
            hive_file_pattern_given: false,
            filename_pattern: None,
            extension: None,
        }
    }
}

impl PartitionOptions {
    /// Takes the option `name`, if it is one of these.
    pub(super) fn take(&mut self, name: &str, value: &Value) -> Result<bool> {
        match name {
            "partition_by" | "partitioned_by" => self.columns = Some(strings(value)?),
            "write_partition_columns" => self.write_columns = is_true(value, name)?,
            "overwrite_or_ignore" | "overwrite" | "append" => {
                if self.overwrite_mode_given {
                    return Err(binder_error(
                        "Can only set one of OVERWRITE_OR_IGNORE, OVERWRITE or APPEND",
                    ));
                }
                self.overwrite_mode_given = true;
                let on = is_true(value, name)?;
                match name {
                    "overwrite_or_ignore" => self.overwrite_or_ignore = on,
                    "overwrite" => self.overwrite = on,
                    _ => {
                        self.append = on;
                        if on && self.filename_pattern.is_none() {
                            self.filename_pattern = Some("{uuid}".to_string());
                        }
                    }
                }
            }
            "hive_file_pattern" => {
                self.hive_file_pattern = is_true(value, name)?;
                self.hive_file_pattern_given = true;
            }
            "filename_pattern" => self.filename_pattern = Some(value.try_to_string()?),
            "file_extension" => self.extension = Some(value.try_to_string()?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `PrepareOutputDirectory`: a directory to write files into, cleared with OVERWRITE.
    fn prepare_directory(&self, path: &str) -> Result<()> {
        let target = std::path::Path::new(path);
        if target.is_file() {
            if self.overwrite {
                std::fs::remove_file(target)
                    .map_err(|e| Error::new(ErrorKind::Io, format!("Could not remove \"{path}\": {e}")))?;
            } else {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "Cannot write to \"{path}\" - it exists and is a file, not a directory! Enable OVERWRITE \
                         option to overwrite the file"
                    ),
                ));
            }
        }
        let occupied = std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some());
        if occupied && !self.overwrite_or_ignore && !self.append {
            if self.overwrite {
                std::fs::remove_dir_all(path)
                    .map_err(|e| Error::new(ErrorKind::Io, format!("Could not clear \"{path}\": {e}")))?;
            } else {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("Directory \"{path}\" is not empty! Enable OVERWRITE option to overwrite files"),
                ));
            }
        }
        Ok(())
    }

    /// `FILE_SIZE_BYTES`, `ROW_GROUPS_PER_FILE` and `PER_THREAD_OUTPUT`: files in the directory of `path`.
    pub(super) fn rotation(
        &self,
        path: &str,
        extension: &str,
        file_size_bytes: Option<usize>,
        row_groups_per_file: Option<usize>,
        per_thread: bool,
    ) -> Result<Option<crate::execution::parquet::Rotation>> {
        if file_size_bytes.is_none() && row_groups_per_file.is_none() && !per_thread {
            return Ok(None);
        }
        if self.columns.is_none() {
            self.prepare_directory(path)?;
            std::fs::create_dir_all(path)
                .map_err(|e| Error::new(ErrorKind::Io, format!("Cannot create directory \"{path}\": {e}")))?;
        }
        Ok(Some(crate::execution::parquet::Rotation {
            file_size_bytes,
            row_groups_per_file,
            per_thread,
            filename_pattern: self.filename_pattern.clone().unwrap_or_else(|| "data_{i}".to_string()),
            extension: self.extension.clone().unwrap_or_else(|| extension.to_string()),
        }))
    }

    /// The partitioning of a COPY to `path` of columns `names`, after clearing the directory if told to.
    /// The partitioning of a COPY to `path`; with ORDER_BY and no PARTITION_BY, all rows are one
    /// partition written to the directory itself.
    pub(super) fn bind_ordered(
        &self,
        path: &str,
        names: &[String],
        extension: &str,
        ordered: bool,
    ) -> Result<Option<Partitioning>> {
        if self.append
            && !self
                .filename_pattern
                .as_deref()
                .is_some_and(|pattern| pattern.contains("{uuid"))
        {
            return Err(binder_error("APPEND mode requires a {uuid} label in filename_pattern"));
        }
        let implicit = Vec::new();
        let wanted = match &self.columns {
            Some(wanted) => wanted,
            None if ordered => &implicit,
            None => return Ok(None),
        };
        if wanted.is_empty() && !ordered {
            return Err(binder_error("PARTITION_BY cannot be empty"));
        }
        let mut columns = Vec::with_capacity(wanted.len());
        let wanted = wanted.clone();
        let star = wanted.len() == 1 && wanted[0] == "*";
        if star {
            columns.extend(0..names.len());
        }
        for column in wanted.iter().filter(|_| !star) {
            let Some(found) = names.iter().position(|n| n.eq_ignore_ascii_case(column)) else {
                return Err(binder_error(format!(
                    "\"partition_by\" expected to find {column}, but it was not found in the table"
                )));
            };
            columns.push(found);
        }
        if !columns.is_empty() && columns.len() == names.len() && !self.write_columns {
            return Err(Error::not_implemented(
                "No column to write as all columns are specified as partition columns. WRITE_PARTITION_COLUMNS \
                 option can be used to write partition columns.",
            ));
        }
        self.prepare_directory(path)?;
        Ok(Some(Partitioning {
            columns,
            write_columns: self.write_columns,
            hive_file_pattern: self.hive_file_pattern,
            path_expression: None,
            claims: Default::default(),
            max_open_files: 100,
            flush_threshold: 524_288,
            filename_pattern: self.filename_pattern.clone().unwrap_or_else(|| "data_{i}".to_string()),
            extension: self.extension.clone().unwrap_or_else(|| extension.to_string()),
            order: Vec::new(),
        }))
    }
}

impl Binder<'_> {
    /// `partitioned_write_max_open_files`.
    pub(super) fn set_max_open_files(&self, partitioning: Option<&mut Partitioning>) {
        let (Some(partitioning), Some(session)) = (partitioning, &self.session) else {
            return;
        };
        if let Ok(value) = (session.setting)("partitioned_write_max_open_files")
            && let Some(limit) = value.as_i128()
        {
            partitioning.max_open_files = limit.max(1) as usize;
        }
        if let Ok(value) = (session.setting)("partitioned_write_flush_threshold")
            && let Some(threshold) = value.as_i128()
        {
            partitioning.flush_threshold = threshold.max(1) as usize;
        }
    }

    /// `PartitionPathBinder`: PARTITION_PATH, over the values of the PARTITION_BY columns.
    pub(super) fn bind_partition_path(
        &mut self,
        info: &CopyInfo,
        names: &[String],
        types: &[LogicalType],
        partition: &PartitionOptions,
        partitioning: Option<&mut Partitioning>,
    ) -> Result<()> {
        use crate::planner::column_binding_resolver::resolve_expression;
        use crate::planner::expression::ColumnBinding;
        use duckrs_parser::ast::ExprKind;
        let Some((_, Some(expression))) = info
            .parsed_options
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("partition_path"))
        else {
            return Ok(());
        };
        let Some(partitioning) = partitioning else {
            return Err(binder_error("PARTITION_PATH requires PARTITION_BY"));
        };
        if partition.hive_file_pattern_given {
            return Err(binder_error(
                "Can't combine HIVE_FILE_PATTERN and PARTITION_PATH for COPY",
            ));
        }
        let columns = partitioning.columns.clone();
        let mut error = None;
        let mut parsed = expression.clone();
        duckrs_parser::transformer::visit_expr_mut(&mut parsed, &mut |expr: &mut Expr| match &expr.kind {
            ExprKind::Subquery { .. } => {
                error.get_or_insert_with(|| "cannot use subquery in PARTITION_PATH".to_string());
            }
            ExprKind::Window(_) => {
                error.get_or_insert_with(|| "window functions are not allowed in PARTITION_PATH".to_string());
            }
            ExprKind::Lambda { .. } => {
                error.get_or_insert_with(|| "lambda functions are not allowed in PARTITION_PATH".to_string());
            }
            ExprKind::ColumnRef(parts) => {
                let known = parts.len() == 1 && columns.iter().any(|&c| names[c] == parts[0]);
                if !known {
                    error.get_or_insert_with(|| {
                        format!(
                            "Column \"{}\" referenced in PARTITION_PATH is not a PARTITION_BY column",
                            parts.join(".")
                        )
                    });
                }
            }
            _ => {}
        });
        if let Some(error) = error {
            return Err(binder_error(error));
        }
        let mut binder = Binder::new(self.catalog);
        binder.options = self.options.clone();
        binder.session = self.session.clone();
        binder.parameters = self.parameters.clone();
        binder.aggregate_error = "aggregate functions are not allowed in PARTITION_PATH";
        let table_index = binder.generate_table_index();
        binder.bind_context.add_binding(Binding {
            alias: String::new(),
            schema: None,
            table_index,
            names: columns.iter().map(|&c| names[c].clone()).collect(),
            types: columns.iter().map(|&c| types[c].clone()).collect(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let mut bound = binder
            .bind_expression(expression)?
            .cast_to(&LogicalType::VARCHAR, false);
        let bindings: Vec<ColumnBinding> = (0..columns.len())
            .map(|column_index| ColumnBinding {
                table_index,
                column_index,
            })
            .collect();
        resolve_expression(&mut bound, &bindings)?;
        partitioning.path_expression = Some(bound);
        Ok(())
    }
}

/// `hive_partitioning`, `hive_types` and `hive_types_autocast` of a multi-file scan.
#[derive(Debug, Clone)]
pub(super) struct HiveOptions {
    pub wanted: Option<bool>,
    pub types: Vec<(String, LogicalType)>,
    pub autocast: bool,
}

impl Default for HiveOptions {
    fn default() -> Self {
        Self {
            wanted: None,
            types: Vec::new(),
            autocast: true,
        }
    }
}

impl Binder<'_> {
    /// `MultiFileOptions::ParseOption`, of the hive options; whether `option` is one.
    pub(super) fn take_hive_option(&mut self, hive: &mut HiveOptions, option: &str, value: &Value) -> Result<bool> {
        let invalid = |message: String| Error::new(ErrorKind::InvalidInput, message);
        match option {
            "hive_partitioning" => hive.wanted = Some(is_true(value, option)?),
            "hive_types_autocast" | "hive_type_autocast" => {
                if value.is_null() {
                    return Err(invalid(format!("Cannot use NULL as argument for {option}")));
                }
                hive.autocast = is_true(value, option)?;
            }
            "hive_types" | "hive_type" => {
                if value.is_null() {
                    return Err(invalid(format!("Cannot use NULL as argument for {option}")));
                }
                if value.logical_type().id() != LogicalTypeId::Struct {
                    return Err(invalid(format!(
                        "'hive_types' only accepts a STRUCT('name':VARCHAR, ...), but '{}' was provided",
                        value.logical_type()
                    )));
                }
                let fields = value.logical_type().struct_children().to_vec();
                let ValueData::Struct(children) = value.data() else {
                    return Err(invalid(format!("Cannot use NULL as argument for {option}")));
                };
                if children.is_empty() {
                    return Err(invalid(
                        "'hive_types' must specify the type of at least one column".to_string(),
                    ));
                }
                for ((name, ty), child) in fields.iter().zip(children) {
                    if ty.id() != LogicalTypeId::Varchar {
                        return Err(invalid(format!(
                            "hive_types: {name} must be a VARCHAR, instead: '{ty}' was provided"
                        )));
                    }
                    let parsed = self.csv_type(&child.try_to_string()?)?;
                    hive.types.retain(|(n, _)| n != name);
                    hive.types.push((name.clone(), parsed));
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// `HivePartitioning::Parse`: the keys of a path, sorted, with their values.
fn sorted_parts(path: &str) -> std::collections::BTreeMap<String, Option<String>> {
    hive_parts(path).into_iter().collect()
}

/// The hive partition columns of `files` (`AutoDetectHivePartitioning` and `MultiFileReader::BindOptions`), by
/// key: also those the files have as a column, which they override.
pub(super) fn hive_partition_columns(files: &[String], options: &HiveOptions) -> Result<Vec<(String, LogicalType)>> {
    let invalid = |message: String| Error::new(ErrorKind::InvalidInput, message);
    let Some(first_file) = files.first() else {
        return Ok(Vec::new());
    };
    let parts: Vec<_> = files.iter().map(|path| sorted_parts(path)).collect();
    let first = &parts[0];
    let types_given = !options.types.is_empty();
    if options.wanted == Some(false) && types_given {
        return Err(invalid(
            "cannot disable hive_partitioning when hive_types is enabled".to_string(),
        ));
    }
    let enabled = match options.wanted {
        Some(wanted) => wanted,
        // hive_types implies hive_partitioning
        None if types_given => true,
        // `AutoDetectHivePartitioningInternal`
        None => {
            !first.is_empty()
                && parts
                    .iter()
                    .all(|file| file.len() == first.len() && file.keys().all(|key| first.contains_key(key)))
        }
    };
    if !enabled {
        return Ok(Vec::new());
    }
    for (path, file) in files.iter().zip(&parts) {
        if let Some(key) = first.keys().find(|key| !file.contains_key(*key)) {
            return Err(binder_error(format!(
                "Hive partition mismatch between file \"{first_file}\" and \"{path}\": key \"{key}\" not found"
            )));
        }
        if file.len() != first.len() {
            return Err(binder_error(format!(
                "Hive partition mismatch between file \"{first_file}\" and \"{path}\""
            )));
        }
    }
    if let Some((name, _)) = options.types.iter().find(|(name, _)| !first.contains_key(name)) {
        return Err(invalid(format!(
            "Unknown hive_type: \"{name}\" does not appear to be a partition"
        )));
    }
    let parsing = Parsing {
        decimal_separator: b'.',
        ..Parsing::default()
    };
    let mut hive = Vec::with_capacity(first.len());
    for key in first.keys() {
        if let Some((_, ty)) = options.types.iter().find(|(name, _)| name == key) {
            hive.push((key.clone(), ty.clone()));
            continue;
        }
        if !options.autocast {
            hive.push((key.clone(), LogicalType::VARCHAR));
            continue;
        }
        // `AutoDetectHiveTypesInternal`: the first candidate each value casts to, VARCHAR where they differ
        let mut detected: Option<LogicalType> = None;
        for value in parts.iter().filter_map(|file| file.get(key).cloned().flatten()) {
            let ty = [LogicalType::DATE, LogicalType::TIMESTAMP, LogicalType::BIGINT]
                .into_iter()
                .find(|ty| can_cast(&value, ty, &parsing))
                .unwrap_or(LogicalType::VARCHAR);
            detected = match detected {
                Some(previous) if previous != ty => Some(LogicalType::VARCHAR),
                _ => Some(ty),
            };
        }
        hive.push((key.clone(), detected.unwrap_or(LogicalType::VARCHAR)));
    }
    Ok(hive)
}

/// The hive partition columns of `files` that are not columns of the files themselves.
pub(super) fn hive_columns(
    files: &[String],
    wanted: Option<bool>,
    names: &[String],
) -> Result<Vec<(String, LogicalType)>> {
    let options = HiveOptions {
        wanted,
        ..HiveOptions::default()
    };
    hive_columns_with(files, &options, names)
}

/// [`hive_partition_columns`], without those the files have as a column.
pub(super) fn hive_columns_with(
    files: &[String],
    options: &HiveOptions,
    names: &[String],
) -> Result<Vec<(String, LogicalType)>> {
    let mut hive = hive_partition_columns(files, options)?;
    hive.retain(|(key, _)| !names.iter().any(|n| n.eq_ignore_ascii_case(key)));
    Ok(hive)
}

pub(super) fn string_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn value_literal(value: &Value) -> Result<String> {
    if let ValueData::List(items) | ValueData::Struct(items) = value.data() {
        let items: Vec<String> = items.iter().map(value_literal).collect::<Result<_>>()?;
        return Ok(format!("[{}]", items.join(", ")));
    }
    match value.logical_type().id() {
        LogicalTypeId::Boolean | LogicalTypeId::Integer | LogicalTypeId::BigInt => value.try_to_string(),
        _ if value.is_null() => Ok("NULL".to_string()),
        _ => Ok(string_literal(&value.try_to_string()?)),
    }
}

/// FORCE_QUOTE: `*`, or the columns that `row(...)` names.
fn quoted_columns(value: &Value, names: &[String]) -> Result<Vec<bool>> {
    let listed = strings(value)?;
    let mut result = vec![false; names.len()];
    for name in &listed {
        if name == "*" {
            result.fill(true);
            continue;
        }
        let Some(position) = names.iter().position(|n| n.eq_ignore_ascii_case(name)) else {
            return Err(binder_error(format!(
                "\"force_quote\" expected to find {name}, but it was not found in the table"
            )));
        };
        result[position] = true;
    }
    Ok(result)
}

/// `OpenerFileSystem::VerifyCanAccessExtension`: what ends with `.duckdb_extension`, or has a
/// `.duckdb_extension.` part, is written only by INSTALL.
fn verify_not_extension(path: &str) -> Result<()> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_lowercase();
    if name.ends_with(".duckdb_extension") || name.contains(".duckdb_extension.") {
        return Err(duckrs_common::Error::new(
            duckrs_common::ErrorKind::Permission,
            format!(
                "File '{path}' cannot be opened for writing since files ending with '.duckdb_extension' are \
                 reserved for DuckDB extensions, and these can only be installed through the INSTALL command"
            ),
        ));
    }
    Ok(())
}

impl Binder<'_> {
    /// `read_text` and `read_blob` (`DirectMultiFileInfo`): a row for each file, with its name, content, size and
    /// modification time. A pattern may match no files.
    pub(super) fn bind_read_file(&mut self, call: &FunctionExpr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let text = name == "read_text";
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        let mut patterns = Vec::new();
        for argument in &args {
            let value = self.constant_argument(&argument.expr)?;
            if !argument.name.is_empty() {
                let option = &argument.name;
                return Err(binder_error(format!(
                    "Invalid named parameter \"{option}\" for function {name}"
                )));
            }
            if value.is_null() {
                return Err(binder_error(format!("{name} cannot take NULL as parameter")));
            }
            patterns.extend(strings(&value)?);
        }
        if patterns.is_empty() {
            return Err(binder_error(format!("{name} requires a file name")));
        }
        let mut files = Vec::new();
        for pattern in &patterns {
            self.check_file_access(pattern)?;
            files.extend(file_system::glob(pattern));
        }
        let content_type = if text { LogicalType::VARCHAR } else { LogicalType::BLOB };
        let mut names = ["filename", "content", "size", "last_modified"]
            .map(String::from)
            .to_vec();
        let hive = hive_columns(&files, None, &names)?;
        let mut rows = Vec::with_capacity(files.len());
        for file in files {
            let mut partitions = Vec::with_capacity(hive.len());
            let parts = hive_parts(&file);
            for (key, ty) in &hive {
                let value = parts
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .and_then(|(_, v)| v.clone());
                let mut strings = duckrs_common::vector::Vector::new(LogicalType::VARCHAR, 1);
                strings.set_value(0, &value.map_or(Value::null(LogicalType::VARCHAR), Value::varchar))?;
                let mut typed = duckrs_common::vector::Vector::new(ty.clone(), 1);
                crate::function::cast::cast_vector(&strings, &mut typed, 1, crate::function::cast::CastMode::Strict)?;
                partitions.push(typed.get_value(0));
            }
            let io = |e: std::io::Error| Error::new(ErrorKind::Io, format!("Cannot open file \"{file}\": {e}"));
            let bytes = std::fs::read(&file).map_err(io)?;
            let metadata = std::fs::metadata(&file).map_err(io)?;
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_micros() as i64)
                .unwrap_or(0);
            let size = bytes.len() as i64;
            let content = if text {
                match String::from_utf8(bytes) {
                    Ok(content) => Value::varchar(content),
                    Err(_) => {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            format!(
                                "read_text: could not read content of file '{file}' as valid UTF-8 encoded text. \
                                 You may want to use read_blob instead."
                            ),
                        ));
                    }
                }
            } else {
                Value::blob(bytes)
            };
            rows.push(vec![
                Value::varchar(file),
                content,
                Value::bigint(size),
                Value::timestamp_tz(duckrs_common::types::Timestamp(modified)),
            ]);
            rows.last_mut().expect("just pushed").extend(partitions);
        }
        let mut types = vec![
            LogicalType::VARCHAR,
            content_type,
            LogicalType::BIGINT,
            LogicalType::new(LogicalTypeId::TimestampTz),
        ];
        for (key, ty) in hive {
            names.push(key);
            types.push(ty);
        }
        self.bind_rows(table_ref, &name, names, types, rows)
    }
}

/// `ParseBytesArg`: a size in bytes, as a number or as text like `'10MB'`.
pub(super) fn parse_bytes_argument(name: &str, value: &Value) -> Result<usize> {
    let upper = name.to_uppercase();
    if value.is_null() {
        return Err(binder_error(format!(
            "NULL is not supported as a valid option for COPY option \"{upper}\""
        )));
    }
    let shown = value.try_to_string()?;
    let result = if value.logical_type().id() == LogicalTypeId::Varchar {
        crate::execution::temporary::parse_memory_limit(&shown)?
    } else {
        match value.as_i128() {
            Some(bytes) if (0..i128::from(u64::MAX)).contains(&bytes) => Some(bytes as u64),
            Some(bytes) if bytes == i128::from(u64::MAX) => None,
            _ => {
                return Err(binder_error(format!(
                    "Unable to parse bytes from \"{shown}\" for copy option \"{upper}\" "
                )));
            }
        }
    };
    match result {
        Some(bytes) => Ok(bytes as usize),
        None => Err(binder_error(format!(
            "Copy option \"{upper}\" must be a valid size, not \"{shown}\""
        ))),
    }
}

/// `RETURN_FILES`, `RETURN_STATS` and `WRITE_EMPTY_FILE`, and the result they make a COPY TO return.
#[derive(Default)]
pub(super) struct OutputOptionsBuilder {
    return_files: bool,
    return_stats: bool,
    write_empty_file: Option<bool>,
}

impl OutputOptionsBuilder {
    pub(super) fn take(&mut self, name: &str, value: &Value) -> Result<bool> {
        match name {
            "return_files" => self.return_files = is_true(value, name)?,
            "return_stats" => self.return_stats = is_true(value, name)?,
            "write_empty_file" => self.write_empty_file = Some(is_true(value, name)?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `ValidateCopyToOptionCombinations`, of these options.
    pub(super) fn build(
        &self,
        partitioned: bool,
        per_thread: bool,
        ordered: bool,
        no_stats_format: Option<&str>,
    ) -> Result<crate::execution::parquet::CopyOutputOptions> {
        if self.return_files && self.return_stats {
            return Err(binder_error(
                "Can only set one of RETURN_FILES or RETURN_STATS for COPY",
            ));
        }
        if self.return_stats
            && let Some(format) = no_stats_format
        {
            return Err(Error::not_implemented(format!(
                "RETURN_STATS is not supported for the \"{format}\" copy format"
            )));
        }
        let write_empty_file = self.write_empty_file.unwrap_or(true);
        if !write_empty_file {
            if per_thread {
                return Err(Error::not_implemented(
                    "Can't combine WRITE_EMPTY_FILE false with PER_THREAD_OUTPUT",
                ));
            }
            if partitioned {
                return Err(Error::not_implemented(
                    "Can't combine WRITE_EMPTY_FILE false with PARTITIONED BY",
                ));
            }
            if ordered {
                return Err(Error::not_implemented(
                    "Can't combine WRITE_EMPTY_FILE false with ORDER BY",
                ));
            }
        }
        Ok(crate::execution::parquet::CopyOutputOptions {
            return_files: self.return_files,
            return_stats: self.return_stats,
            write_empty_file,
        })
    }
}

/// What a COPY TO returns: its `Count`, and with RETURN_FILES its `Files`.
pub(super) fn copy_result(output: &crate::execution::parquet::CopyOutputOptions) -> (Vec<String>, StatementReturnType) {
    if output.return_stats {
        return (
            crate::execution::parquet::CopyOutputOptions::stats_names(),
            StatementReturnType::QueryResult,
        );
    }
    if output.return_files {
        (
            vec!["Count".to_string(), "Files".to_string()],
            StatementReturnType::QueryResult,
        )
    } else {
        (vec!["Count".to_string()], StatementReturnType::ChangedRows)
    }
}

/// The names COPY TO writes: `QueryResult::DeduplicateColumns` of the select list.
pub(super) fn unique_names(names: &[String]) -> Vec<String> {
    let mut names = names.to_vec();
    super::star::deduplicate_columns(&mut names);
    names
}

fn missing_column_types(column: &str) -> Error {
    binder_error(format!(
        "COLUMN_TYPES error: Columns with names: \"{column}\" do not exist in the CSV File"
    ))
}

/// `CSVSchema::ReplaceNullWithVarchar`: a column nothing says anything of.
fn replace_null_with_varchar(types: &mut [LogicalType]) {
    for ty in types {
        if ty.id() == LogicalTypeId::SqlNull {
            *ty = LogicalType::VARCHAR;
        }
    }
}

/// `CSVSchema::CanWeCastIt`, for the types the sniffer detects.
fn can_we_cast_it(source: LogicalTypeId, destination: LogicalTypeId) -> bool {
    use LogicalTypeId as T;
    if destination == T::Varchar || source == destination {
        return true;
    }
    match source {
        T::SqlNull => true,
        T::TinyInt => matches!(
            destination,
            T::SmallInt | T::Integer | T::BigInt | T::Decimal | T::HugeInt | T::Bignum | T::Float | T::Double
        ),
        T::SmallInt => matches!(
            destination,
            T::Integer | T::BigInt | T::Decimal | T::HugeInt | T::Bignum | T::Float | T::Double
        ),
        T::Integer => matches!(
            destination,
            T::BigInt | T::Decimal | T::HugeInt | T::Bignum | T::Float | T::Double
        ),
        T::BigInt => matches!(destination, T::Decimal | T::HugeInt | T::Bignum | T::Float | T::Double),
        T::HugeInt | T::UHugeInt => matches!(destination, T::Bignum | T::Double),
        T::Bignum | T::Float => destination == T::Double,
        _ => false,
    }
}

/// `CSVSchema::MergeSchemas`, of the types (null padding does not add columns: the scan has the first's).
fn merge_schema_types(types: &mut [LogicalType], other: &[LogicalType]) {
    for (this, other) in types.iter_mut().zip(other) {
        if this == other {
            continue;
        }
        let (this_id, other_id) = (this.id(), other.id());
        if can_we_cast_it(this_id, other_id) {
            *this = other.clone();
        } else if !can_we_cast_it(other_id, this_id) {
            let candidates = [
                LogicalType::BOOLEAN,
                LogicalType::BIGINT,
                LogicalType::HUGEINT,
                LogicalType::new(LogicalTypeId::Bignum),
                LogicalType::DOUBLE,
                LogicalType::VARCHAR,
            ];
            if let Some(ty) = candidates
                .into_iter()
                .find(|ty| can_we_cast_it(this_id, ty.id()) && can_we_cast_it(other_id, ty.id()))
            {
                *this = ty;
            }
        }
    }
}

fn buffer_below_line_size(buffer: usize, line: usize) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("Buffer Size of {buffer} must be a higher value than the maximum line size {line}"),
    )
}

/// The options every COPY TO has (`GetFullCopyOptionsList`).
const GENERIC_COPY_OPTIONS: &[&str] = &[
    "use_tmp_file",
    "overwrite_or_ignore",
    "overwrite",
    "append",
    "filename_pattern",
    "file_extension",
    "per_thread_output",
    "row_group_size",
    "row_group_size_bytes",
    "row_groups_per_file",
    "file_size_bytes",
    "partition_by",
    "order_by",
    "return_files",
    "preserve_order",
    "return_stats",
    "return_column_types",
    "write_partition_columns",
    "write_empty_file",
    "hive_file_pattern",
];

/// `CSVListCopyOptions`.
const CSV_COPY_OPTIONS: &[&str] = &[
    "auto_detect",
    "sample_size",
    "skip",
    "max_line_size",
    "maximum_line_size",
    "ignore_errors",
    "buffer_size",
    "decimal_separator",
    "null_padding",
    "parallel",
    "allow_quoted_nulls",
    "store_rejects",
    "force_not_null",
    "rejects_table",
    "rejects_scan",
    "rejects_limit",
    "encoding",
    "thousands",
    "force_quote",
    "prefix",
    "suffix",
    "new_line",
    "date_format",
    "dateformat",
    "timestamp_format",
    "timestampformat",
    "quote",
    "comment",
    "delim",
    "delimiter",
    "sep",
    "separator",
    "escape",
    "header",
    "nullstr",
    "null",
    "compression",
    "strict_mode",
];

/// Whether a COPY function with the options `own` has `option`, its own or one every COPY has.
pub(super) fn is_copy_option(own: &[&str], option: &str) -> bool {
    let option = option.to_lowercase();
    own.iter().chain(GENERIC_COPY_OPTIONS).any(|known| *known == option)
}

/// `BindCopyOptions`: an option the COPY function does not have, with the closest ones it does.
pub(super) fn unrecognized_copy_option(option: &str, format: &str, own: &[&str]) -> Error {
    let target = option.to_lowercase();
    let mut names: Vec<&str> = own.iter().chain(GENERIC_COPY_OPTIONS).copied().collect();
    names.sort_unstable();
    names.dedup();
    let scores = names
        .iter()
        .map(|candidate| {
            (
                candidate.to_string(),
                duckrs_common::string_util::similarity_rating(candidate, &target),
            )
        })
        .collect();
    let candidates = duckrs_common::string_util::top_n_strings(scores);
    Error::new(
        ErrorKind::NotImplemented,
        format!(
            "Unrecognized option {option} for {format}\n{}",
            duckrs_common::string_util::candidates_message(&candidates, "Candidate options")
        ),
    )
}
