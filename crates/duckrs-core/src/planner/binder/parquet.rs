// Ported from duckdb/extension/parquet/parquet_extension.cpp @ rust-port-baseline
//
//! `read_parquet` and COPY with FORMAT PARQUET.

use super::copy::{
    OutputOptionsBuilder, PartitionOptions, copy_result, is_true, parse_bytes_argument, string_literal, strings,
};
use super::table_functions::{named_argument, parse_select};
use super::{Binder, Binding, BoundStatement, binder_error};
use crate::execution::file_system;
use crate::execution::parquet::{
    ParquetScanData, ParquetWriteOptions, file_metadata_row, file_schema, key_value_rows, metadata_rows,
    parse_compression, schema_table_rows,
};
use crate::execution::parquet_writer::{FieldIds, MAX_UNCOMPRESSED_DICT_PAGE_SIZE, MAX_UNCOMPRESSED_PAGE_SIZE};
use crate::function::combine_types::force_max_logical_type;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{CopyInfo, FunctionExpr, InsertStatement, InsertValues, TableRef};
use std::sync::Arc;

const METADATA_COLUMNS: &str = "file_name, row_group_id, row_group_num_rows, row_group_num_columns, row_group_bytes, \
     column_id, file_offset, num_values, path_in_schema, \"type\", stats_min, stats_max, stats_null_count, \
     stats_distinct_count, stats_min_value, stats_max_value, compression, encodings, index_page_offset, \
     dictionary_page_offset, data_page_offset, total_compressed_size, total_uncompressed_size, key_value_metadata, \
     bloom_filter_offset, bloom_filter_length, min_is_exact, max_is_exact, row_group_compressed_bytes, geo_bbox, \
     geo_types, encoding_stats, stats_nan_count, column_index_offset, column_index_length, offset_index_offset, \
     offset_index_length";
const METADATA_EMPTY: &str = "(NULL::VARCHAR, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, \
     NULL::BIGINT, NULL::BIGINT, NULL::VARCHAR, NULL::VARCHAR, NULL::VARCHAR, NULL::VARCHAR, NULL::BIGINT, NULL::BIGINT, \
     NULL::VARCHAR, NULL::VARCHAR, NULL::VARCHAR, NULL::VARCHAR, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, \
     NULL::BIGINT, NULL::MAP(BLOB, BLOB), NULL::BIGINT, NULL::BIGINT, NULL::BOOLEAN, NULL::BOOLEAN, NULL::BIGINT, \
     NULL::STRUCT(xmin DOUBLE, xmax DOUBLE, ymin DOUBLE, ymax DOUBLE, zmin DOUBLE, zmax DOUBLE, mmin DOUBLE, \
     mmax DOUBLE), NULL::VARCHAR[], NULL::STRUCT(page_type VARCHAR, encoding VARCHAR, count INTEGER)[], NULL::BIGINT, \
     NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT)";
const PAGE_INDEX_COLUMNS: &str = "file_name, row_group_id, column_id, path_in_schema, page_id, page_offset, \
     compressed_page_size, first_row_index, num_rows, null_page, min_value, max_value, null_count, boundary_order";
const PAGE_INDEX_EMPTY: &str = "(NULL::VARCHAR, NULL::BIGINT, NULL::BIGINT, NULL::VARCHAR, NULL::BIGINT, NULL::BIGINT, \
     NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::BOOLEAN, NULL::VARCHAR, NULL::VARCHAR, NULL::BIGINT, NULL::VARCHAR)";
const SCHEMA_COLUMNS: &str = "file_name, name, \"type\", type_length, repetition_type, num_children, converted_type, \
     scale, \"precision\", field_id, logical_type, duckdb_type, column_id";
const SCHEMA_EMPTY: &str = "(NULL::VARCHAR, NULL::VARCHAR, NULL::VARCHAR, NULL::VARCHAR, NULL::VARCHAR, NULL::BIGINT, \
     NULL::VARCHAR, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::VARCHAR, NULL::VARCHAR, NULL::BIGINT)";
const KEY_VALUE_COLUMNS: &str = "file_name, \"key\", \"value\"";
const KEY_VALUE_EMPTY: &str = "(NULL::VARCHAR, NULL::BLOB, NULL::BLOB)";
const FILE_METADATA_COLUMNS: &str = "file_name, created_by, num_rows, num_row_groups, format_version, \
     encryption_algorithm, footer_signing_key_metadata, file_size_bytes, footer_size, column_orders";
const FILE_METADATA_EMPTY: &str = "(NULL::VARCHAR, NULL::VARCHAR, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::VARCHAR, \
     NULL::VARCHAR, NULL::UBIGINT, NULL::UBIGINT, NULL::VARCHAR[])";

impl Binder<'_> {
    /// `ParquetScanFunction::ParquetScanBind`.
    /// `read_single_csv_file` and `read_single_parquet_file`: the reader of one file that the multi-file
    /// readers wrap (`TableFunctionMultiFile`), here the multi-file reader of that one file.
    pub(super) fn bind_read_single_file(
        &mut self,
        call: &FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        if let Some(path) = args.iter().find(|argument| argument.name.is_empty()) {
            let value = self.constant_argument(&path.expr)?;
            if !matches!(
                value.logical_type().id(),
                LogicalTypeId::Varchar | LogicalTypeId::Struct
            ) {
                return Err(binder_error(format!(
                    "{name} reads a single file - given as a VARCHAR path or as a file struct, not as {}",
                    value.logical_type()
                )));
            }
            if value.logical_type().id() == LogicalTypeId::Varchar {
                let path = value.try_to_string()?;
                if path.is_empty() {
                    return Err(binder_error(format!("{name} requires a non-empty file name")));
                }
                if !std::path::Path::new(&path).is_file() {
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!("Cannot open file \"{path}\": No such file or directory"),
                    ));
                }
            }
        }
        let mut call = call.clone();
        call.name.name = if name == "read_single_csv_file" {
            "read_csv".to_string()
        } else {
            "read_parquet".to_string()
        };
        if name == "read_single_csv_file" {
            self.bind_read_csv(&call, table_ref)
        } else {
            self.bind_read_parquet(&call, table_ref)
        }
    }

    pub(super) fn bind_read_parquet(&mut self, call: &FunctionExpr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        let mut paths = None;
        let (mut binary_as_string, mut union_by_name, mut file_row_number) = (false, false, false);
        let mut filename = None;
        let mut hive_options = super::copy::HiveOptions::default();
        let mut int96_as = crate::execution::parquet::Int96As::Timestamp;
        let mut utf8_validation = crate::execution::parquet::Utf8Validation::Strict;
        let mut schema_value = None;
        let mut allow_empty = false;
        let (mut copy_names, mut copy_types): (Option<Vec<String>>, Option<Vec<LogicalType>>) = (None, None);
        for argument in &args {
            let value = self.constant_argument(&argument.expr)?;
            if argument.name.is_empty() {
                if paths.is_some() {
                    return Err(binder_error(format!("{name} takes one positional parameter")));
                }
                if value.is_null() {
                    return Err(binder_error(format!("{name} cannot take NULL list as parameter")));
                }
                paths = Some(strings(&value)?);
                continue;
            }
            let option = argument.name.to_lowercase();
            if self.take_hive_option(&mut hive_options, &option, &value)? {
                continue;
            }
            match option.as_str() {
                "binary_as_string" => binary_as_string = is_true(&value, &option)?,
                "union_by_name" => union_by_name = is_true(&value, &option)?,
                "file_row_number" => file_row_number = is_true(&value, &option)?,
                "int96_as" => {
                    use crate::execution::parquet::Int96As;
                    let text = value.try_to_string()?;
                    int96_as = match text.to_lowercase().as_str() {
                        "timestamp" => Int96As::Timestamp,
                        "timestamp_ns" => Int96As::TimestampNs,
                        "struct" => Int96As::Struct,
                        _ => {
                            return Err(binder_error(format!(
                                "Unrecognized int96_as '{text}' (supported: 'timestamp', 'timestamp_ns', 'struct')"
                            )));
                        }
                    };
                }
                "utf8_validation" => {
                    if value.is_null() {
                        return Err(binder_error("Parquet utf8_validation cannot be empty!"));
                    }
                    utf8_validation = crate::execution::parquet::Utf8Validation::parse(&value.try_to_string()?)?;
                }
                "allow_empty" => allow_empty = is_true(&value, &option)?,
                "explicit_cardinality" | "can_have_nan" => {}
                "schema" => schema_value = Some(value),
                // what COPY ... FROM passes on of the table it loads into
                "__copy_names" => copy_names = Some(strings(&value)?),
                "__copy_types" => {
                    copy_types = Some(
                        strings(&value)?
                            .iter()
                            .map(|name| self.csv_type(name))
                            .collect::<Result<_>>()?,
                    );
                }
                // options of COPY TO, which reading doesn't have
                "write_page_index"
                | "write_bloom_filter"
                | "row_group_size"
                | "row_group_size_bytes"
                | "compression"
                | "compression_level"
                | "parquet_version"
                | "kv_metadata"
                | "dictionary_size_limit"
                | "data_page_size_limit" => {
                    return Err(binder_error(format!(
                        "Invalid named parameter \"{option}\" for function {name}"
                    )));
                }
                "filename" => {
                    filename = super::copy::filename_option(&value, &option)?;
                }
                _ => return Err(crate::unported(format!("the {name} option {option}"))),
            }
        }
        let patterns = paths.ok_or_else(|| binder_error(format!("{name} requires a file name")))?;
        let mut files = Vec::new();
        for pattern in &patterns {
            self.check_file_access(pattern)?;
            let found = file_system::glob_or_directory(pattern, "parquet");
            if found.is_empty() && !allow_empty {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            files.extend(found);
        }
        // `allow_empty` and no file: one column, and nothing in it
        let empty = files.is_empty() && allow_empty;
        if files.is_empty() && !empty {
            return Err(binder_error(format!("{name} needs at least one file to read")));
        }
        let schema = match &schema_value {
            Some(value) if !empty => {
                if union_by_name {
                    return Err(binder_error("'schema' cannot be combined with union_by_name=true"));
                }
                let try_cast = |value: &Value, ty: &LogicalType| crate::execution::parquet::try_cast_value(value, ty);
                let mut parse_type = |name: &str| self.csv_type(name);
                let columns = crate::execution::parquet_schema::SchemaParser {
                    parse_type: &mut parse_type,
                    try_cast: &try_cast,
                }
                .parse(value)?;
                crate::execution::parquet::check_schema(&files[0], binary_as_string, int96_as, &columns)?;
                Some(Arc::new(columns))
            }
            _ => None,
        };
        let (mut names, mut types) = match &schema {
            None if empty => (vec!["empty".to_string()], vec![LogicalType::BOOLEAN]),
            Some(columns) => (
                columns.iter().map(|c| c.name.clone()).collect(),
                columns.iter().map(|c| c.ty.clone()).collect(),
            ),
            None => file_schema(&files[0], binary_as_string, int96_as)?,
        };
        if union_by_name && !empty {
            for path in &files[1..] {
                let (file_names, file_types) = file_schema(path, binary_as_string, int96_as)?;
                for (column, ty) in file_names.iter().zip(&file_types) {
                    match names.iter().position(|n| n.eq_ignore_ascii_case(column)) {
                        Some(i) => types[i] = force_max_logical_type(&types[i], ty),
                        None => {
                            names.push(column.clone());
                            types.push(ty.clone());
                        }
                    }
                }
            }
        }
        // COPY ... FROM: the columns of the table are the types to read (`return_types` of the bind)
        let mut table_columns = Vec::new();
        if let (Some(copy_names), Some(copy_types)) = (&copy_names, &copy_types) {
            if copy_types.len() != types.len() {
                let schema_text = |names: &[String], types: &[LogicalType]| {
                    names
                        .iter()
                        .zip(types)
                        .map(|(name, ty)| format!("{name} {ty}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let file_string = if union_by_name {
                    files.join(",")
                } else {
                    files[0].clone()
                };
                return Err(Error::new(
                    ErrorKind::Conversion,
                    format!(
                        "Failed to read file(s) \"{file_string}\" - column count mismatch: expected {} columns but \
                         found {}\nTable schema: {}\nParquet schema: {}\n\nPossible solutions:\n* Manually specify \
                         which columns to insert using \"INSERT INTO tbl SELECT ... FROM read_parquet(...)\"",
                        copy_types.len(),
                        types.len(),
                        schema_text(copy_names, copy_types),
                        schema_text(&names, &types),
                    ),
                ));
            }
            types = copy_types.clone();
            table_columns = copy_names.clone();
        }
        let mut output_names = names.clone();
        let mut output_types = types.clone();
        if file_row_number {
            output_names.push("file_row_number".to_string());
            output_types.push(LogicalType::BIGINT);
        }
        if let Some(column) = &filename {
            output_names.push(column.clone());
            output_types.push(LogicalType::VARCHAR);
        }
        let mut hive = Vec::new();
        let mut hive_overrides = Vec::new();
        // with `schema`, hive partitioning is asked for (hive_partitioning or hive_types) rather than detected
        if schema.is_some() && hive_options.wanted.is_none() && hive_options.types.is_empty() {
            hive_options.wanted = Some(false);
        }
        for (key, ty) in super::copy::hive_partition_columns(&files, &hive_options)? {
            match names.iter().position(|n| n.eq_ignore_ascii_case(&key)) {
                // a column of the file as well: the path gives its values, and its type
                Some(column) => {
                    if let Some(columns) = &schema
                        && columns[column].ty != ty
                    {
                        return Err(binder_error(format!(
                            "'schema' declares column \"{}\" as {}, but its hive partition type is {ty} - set the \
                             partition type with hive_types",
                            columns[column].name, columns[column].ty
                        )));
                    }
                    types[column] = ty.clone();
                    output_types[column] = ty;
                    hive_overrides.push((column, key));
                }
                None => {
                    output_names.push(key.clone());
                    output_types.push(ty.clone());
                    hive.push((key, ty));
                }
            }
        }
        let alias = if table_ref.alias.is_empty() {
            name
        } else {
            table_ref.alias.clone()
        };
        if schema.is_some() {
            for (i, name) in output_names.iter().enumerate() {
                if output_names[..i].iter().any(|other| other.eq_ignore_ascii_case(name)) {
                    return Err(binder_error(format!(
                        "table \"{alias}\" has duplicate column name \"{name}\""
                    )));
                }
            }
        }
        let mut output_names = Self::alias_columns(table_ref, &alias, output_names)?;
        // `Binder::BindTableFunctionInternal`: a column without a name is `C` and its index
        for (i, name) in output_names.iter_mut().enumerate() {
            if name.is_empty() {
                *name = format!("C{i}");
            }
        }
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
        let scan = ParquetScanData {
            files,
            names,
            types,
            binary_as_string,
            int96_as,
            utf8_validation,
            table_columns,
            union_by_name,
            file_row_number,
            filename: filename.is_some(),
            hive,
            virtual_columns,
            schema,
            read: None,
            hive_overrides,
            function_name: call.name.name.to_lowercase(),
            arguments: format!("{:?}", call.args),
            filters: Vec::new(),
            projected: None,
            file_filters: String::new(),
            column_order: self.column_order(table_index),
            total_files: None,
            extracts: Vec::new(),
        };
        Ok(LogicalOperator::new(
            LogicalOperatorKind::ParquetScan {
                table_index,
                scan: Arc::new(scan),
            },
            vec![],
        ))
    }

    /// `parquet_metadata` and its kin, as a list of constants for each file.
    pub(super) fn bind_parquet_metadata(
        &mut self,
        call: &FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        if name == "parquet_bloom_probe" {
            let [file, column, value] = args.as_slice() else {
                return Err(binder_error("parquet_bloom_probe takes a file, a column and a value"));
            };
            let (file, column, value) = (
                self.constant_argument(&file.expr)?,
                self.constant_argument(&column.expr)?,
                self.constant_argument(&value.expr)?,
            );
            if file.is_null() || column.is_null() || value.is_null() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Can't have NULL parameters for parquet_bloom_probe",
                ));
            }
            let column = column.try_to_string()?;
            let mut rows = vec!["(NULL::VARCHAR, NULL::BIGINT, NULL::BOOLEAN)".to_string()];
            for pattern in strings(&file)? {
                self.check_file_access(&pattern)?;
                let found = file_system::glob(&pattern);
                if found.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!("No files found that match the pattern \"{pattern}\""),
                    ));
                }
                for path in found {
                    rows.extend(crate::execution::parquet::bloom_probe_rows(&path, &column, &value)?);
                }
            }
            let select = parse_select(
                &format!(
                    "SELECT * FROM (VALUES {}) t(file_name, row_group_id, bloom_filter_excludes) OFFSET 1",
                    rows.join(", ")
                ),
                "Expected a single SELECT statement",
            )?;
            return self.bind_rewritten(&select, table_ref, &name);
        }
        let [argument] = args.as_slice() else {
            return Err(binder_error(format!("{name} takes one positional parameter")));
        };
        let value = self.constant_argument(&argument.expr)?;
        let mut files = Vec::new();
        for pattern in strings(&value)? {
            self.check_file_access(&pattern)?;
            let found = file_system::glob(&pattern);
            if found.is_empty() {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            files.extend(found);
        }
        // `FullMetadataProcessor`: a row per file, with the rows of each of the others as a list
        if name == "parquet_full_metadata" {
            let parts: Vec<String> = files
                .iter()
                .map(|file| {
                    // the errors of the file are those of reading its metadata
                    metadata_rows(file)?;
                    let path = string_literal(file);
                    let list = |function: &str| {
                        format!("(SELECT coalesce(list(t), []) FROM {function}({path}) t) AS {function}")
                    };
                    Ok(format!(
                        "SELECT {}, {}, {}, {}",
                        list("parquet_file_metadata"),
                        list("parquet_metadata"),
                        list("parquet_schema"),
                        list("parquet_kv_metadata")
                    ))
                })
                .collect::<Result<_>>()?;
            let select = parse_select(&parts.join(" UNION ALL "), "Expected a single SELECT statement")?;
            return self.bind_rewritten(&select, table_ref, &name);
        }
        let (columns, empty) = match name.as_str() {
            "parquet_metadata" => (METADATA_COLUMNS, METADATA_EMPTY),
            "parquet_page_index" => (PAGE_INDEX_COLUMNS, PAGE_INDEX_EMPTY),
            "parquet_schema" => (SCHEMA_COLUMNS, SCHEMA_EMPTY),
            "parquet_kv_metadata" => (KEY_VALUE_COLUMNS, KEY_VALUE_EMPTY),
            _ => (FILE_METADATA_COLUMNS, FILE_METADATA_EMPTY),
        };
        let mut rows = Vec::new();
        for file in &files {
            match name.as_str() {
                "parquet_metadata" => rows.extend(metadata_rows(file)?),
                "parquet_page_index" => rows.extend(crate::execution::parquet::page_index_rows(file)?),
                "parquet_schema" => rows.extend(schema_table_rows(file)?),
                "parquet_kv_metadata" => rows.extend(key_value_rows(file)?),
                _ => rows.push(file_metadata_row(file)?),
            }
        }
        // the first row fixes the types, and says nothing itself
        rows.insert(0, empty.to_string());
        let select = parse_select(
            &format!("SELECT * FROM (VALUES {}) t({columns}) OFFSET 1", rows.join(", ")),
            "Expected a single SELECT statement",
        )?;
        self.bind_rewritten(&select, table_ref, &name)
    }

    /// The path of a COPY that is about Parquet.
    pub(super) fn parquet_copy_path(&mut self, info: &CopyInfo) -> Result<Option<String>> {
        let path = match &info.file_path_expression {
            Some(expression) => self.constant_argument(expression)?.try_to_string()?,
            None => info.file_path.clone(),
        };
        let parquet = if info.format.is_empty() {
            path.to_lowercase().ends_with(".parquet")
        } else {
            info.format.eq_ignore_ascii_case("parquet")
        };
        Ok(parquet.then_some(path))
    }

    /// COPY FROM with FORMAT PARQUET: an INSERT of the columns of the file, in their order.
    pub(super) fn bind_copy_from_parquet(&mut self, info: &CopyInfo, path: &str) -> Result<BoundStatement> {
        let table = self
            .catalog
            .get_table(self.schema_of(&info.table)?.as_deref(), &info.table.name)?;
        let columns: Vec<String> = if info.select_list.is_empty() {
            table.columns.iter().map(|c| c.name.clone()).collect()
        } else {
            info.select_list.clone()
        };
        // `ParseCopyOption`: the options of reading go to the scan, as do the columns it loads into
        let types: Vec<String> = columns
            .iter()
            .map(|name| {
                table
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                    .map(|c| c.ty.to_string())
                    .ok_or_else(|| {
                        binder_error(format!(
                            "Table \"{}\" does not have a column named \"{name}\"",
                            info.table.name
                        ))
                    })
            })
            .collect::<Result<_>>()?;
        let list = |items: &[String]| {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|item| string_literal(item))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let mut arguments = format!(", __copy_names := {}, __copy_types := {}", list(&columns), list(&types));
        for (name, value) in self.copy_options(info)? {
            match name.as_str() {
                "format" | "compression" | "codec" | "row_group_size" | "debug_use_openssl" | "prefetch_strategy" => {}
                "binary_as_string" | "can_have_nan" | "utf8_validation" | "int96_as" => {
                    if value.is_null() {
                        return Err(binder_error(format!("Parquet {name} cannot be empty!")));
                    }
                    let literal = match value.logical_type().id() {
                        LogicalTypeId::Boolean => value.try_to_string()?,
                        _ => string_literal(&value.try_to_string()?),
                    };
                    arguments.push_str(&format!(", {name} := {literal}"));
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::NotImplemented,
                        format!("Unsupported option for COPY FROM: {name}"),
                    ));
                }
            }
        }
        let select = parse_select(
            &format!(
                "SELECT * FROM read_parquet({}, hive_partitioning := false{arguments})",
                string_literal(path)
            ),
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

    /// `ParquetWriteBind`.
    pub(super) fn bind_copy_to_parquet(&mut self, info: &CopyInfo, path: String) -> Result<BoundStatement> {
        let select = self.copy_to_select(info)?;
        let bound = self.bind_select_statement(&select)?;
        let mut options = ParquetWriteOptions::default();
        let writer = &mut options.writer;
        let mut compression = None;
        let mut level = None;
        let mut partition = PartitionOptions::default();
        let mut page_size_limit = None;
        let (mut file_size_bytes, mut row_groups_per_file, mut per_thread) = (None, None, false);
        let mut output = OutputOptionsBuilder::default();
        let ordered = info
            .parsed_options
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("order_by"));
        let mut field_ids = None;
        let mut shredding = None;
        for (name, value) in self.copy_options(info)? {
            if partition.take(&name, &value)? || output.take(&name, &value)? {
                continue;
            }
            let number = || -> Result<i64> {
                value
                    .try_to_string()?
                    .parse()
                    .map_err(|_| binder_error(format!("{name} must be an integer")))
            };
            let bytes = || -> Result<usize> {
                match value.as_i128() {
                    Some(n) => Ok(n.max(0) as usize),
                    None => Ok(
                        crate::execution::temporary::parse_memory_limit(&value.try_to_string()?)?.unwrap_or(u64::MAX)
                            as usize,
                    ),
                }
            };
            match name.as_str() {
                "format" | "use_tmp_file" | "preserve_order" | "order_by" => {}
                "compression" | "codec" => {
                    if value.logical_type().id() == LogicalTypeId::Boolean {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            format!("Copy option \"{name}\" requires an argument"),
                        ));
                    }
                    if value.logical_type().id() != LogicalTypeId::Varchar {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            format!(
                                "Copy option \"{name}\" expected an argument of type VARCHAR - the argument \"{}\" of \
                                 type {} could not be cast as this type",
                                value.try_to_string()?,
                                value.logical_type()
                            ),
                        ));
                    }
                    compression = Some((name.clone(), value.try_to_string()?));
                }
                "compression_level" => level = Some(number()? as i32),
                "row_group_size" | "chunk_size" => {
                    if value.as_i128().is_some_and(|n| n >= i128::from(u64::MAX)) {
                        return Err(binder_error(format!(
                            "BATCH_SIZE/ROW_GROUP_SIZE value {} is too large",
                            value.try_to_string()?
                        )));
                    }
                    writer.row_group_size = number()?.max(1) as usize;
                }
                "row_group_size_bytes" => writer.row_group_size_bytes = Some(bytes()?),
                "bloom_filter_fallback_max_bytes" => {
                    writer.bloom_filter_fallback_max_bytes = parse_bytes_argument(&name, &value)?;
                }
                "file_size_bytes" => file_size_bytes = Some(parse_bytes_argument(&name, &value)?),
                "row_groups_per_file" | "batches_per_file" => {
                    let count = number()?;
                    if count <= 0 {
                        return Err(binder_error(
                            "BATCHES_PER_FILE/ROW_GROUPS_PER_FILE must be a positive number",
                        ));
                    }
                    row_groups_per_file = Some(count as usize);
                }
                "per_thread_output" => per_thread = is_true(&value, &name)?,
                "dictionary_compression_ratio_threshold" | "debug_use_openssl" => {}
                "dictionary_size_limit" => {
                    let limit = number()?;
                    if limit < 0 {
                        return Err(binder_error(
                            "dictionary_size_limit must be greater than 0 or 0 to disable",
                        ));
                    }
                    writer.dictionary_size_limit = Some(limit as usize);
                }
                "string_dictionary_page_size_limit" => {
                    let limit = number()?;
                    if limit <= 0 || limit as usize > MAX_UNCOMPRESSED_DICT_PAGE_SIZE {
                        return Err(binder_error(format!(
                            "string_dictionary_page_size_limit cannot be 0 and must be less than or equal to \
                             {MAX_UNCOMPRESSED_DICT_PAGE_SIZE}"
                        )));
                    }
                    writer.string_dictionary_page_size_limit = limit as usize;
                }
                "data_page_size_limit" => {
                    let limit = number()?;
                    if limit <= 0 || limit as usize > MAX_UNCOMPRESSED_PAGE_SIZE {
                        return Err(binder_error(format!(
                            "data_page_size_limit cannot be 0 and must be less than or equal to \
                             {MAX_UNCOMPRESSED_PAGE_SIZE}"
                        )));
                    }
                    page_size_limit = Some(limit as usize);
                }
                "write_bloom_filter" => writer.bloom_filters = is_true(&value, &name)?,
                "write_page_index" => writer.write_page_index = is_true(&value, &name)?,
                "field_ids" => field_ids = Some(value.clone()),
                "shredding" => shredding = Some(value.clone()),
                "bloom_filter_false_positive_ratio" => {
                    let ratio: f64 = value
                        .try_to_string()?
                        .parse()
                        .map_err(|_| binder_error("bloom_filter_false_positive_ratio must be a number"))?;
                    if !(ratio > 0.0 && ratio <= 1.0) {
                        return Err(binder_error(
                            "bloom_filter_false_positive_ratio must be greater than 0 and at most 1",
                        ));
                    }
                    writer.bloom_filter_fpp = ratio;
                }
                "write_timestamp_as_int96" => writer.timestamp_as_int96 = is_true(&value, &name)?,
                "parquet_version" => {
                    // an option without a value is `true`, which is no VARCHAR
                    if value.logical_type().id() == LogicalTypeId::Boolean {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            format!(
                                "Copy option \"{name}\" expected an argument of type VARCHAR - the argument \"true\" of \
                                 type BOOLEAN could not be cast as this type"
                            ),
                        ));
                    }
                    writer.version = match value.try_to_string()?.to_uppercase().as_str() {
                        "V1" => 1,
                        "V2" => 2,
                        _ => return Err(binder_error("Expected parquet_version 'V1' or 'V2'")),
                    };
                }
                "kv_metadata" => {
                    let ValueData::Struct(values) = value.data() else {
                        return Err(binder_error("Expected kv_metadata argument to be a STRUCT"));
                    };
                    for ((key, _), item) in value.logical_type().struct_children().iter().zip(values) {
                        if item.is_null() {
                            // the parameters of a prepared COPY are NULL until it runs
                            if self.parameters.is_some() {
                                continue;
                            }
                            return Err(binder_error(format!(
                                "kv_metadata value for key \"{key}\" cannot be NULL"
                            )));
                        }
                        // a BLOB is its bytes, the rest their text
                        let bytes = match item.data() {
                            ValueData::Bytes(raw) if item.logical_type().id() == LogicalTypeId::Blob => raw.clone(),
                            _ => Value::try_to_string(item)?.into_bytes(),
                        };
                        writer.key_values.push((key.clone(), bytes));
                    }
                }
                _ if !super::copy::is_copy_option(PARQUET_COPY_OPTIONS, &name) => {
                    return Err(super::copy::unrecognized_copy_option(
                        &name,
                        "parquet",
                        PARQUET_COPY_OPTIONS,
                    ));
                }
                _ => return Err(crate::unported(format!("the COPY option {name} for Parquet"))),
            }
        }
        if let Some((option, name)) = compression {
            writer.codec = parse_compression(&name, level).map_err(|_| {
                binder_error(format!(
                    "Expected \"{option}\" argument to be any of [uncompressed, brotli, gzip, snappy, lz4, lz4_raw or \
                     zstd]"
                ))
            })?;
        }
        // `DEFAULT_DATA_PAGE_SIZE_LIMIT`: with the page index, pages of 1 MiB; without, one per column chunk
        writer.data_page_size_limit = page_size_limit.unwrap_or(if writer.write_page_index {
            1_048_576
        } else {
            MAX_UNCOMPRESSED_PAGE_SIZE
        });
        let order = self.bind_copy_order(info, &bound.names, &bound.types)?;
        options.partitioning = partition.bind_ordered(&path, &bound.names, "parquet", !order.is_empty())?;
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
        if let Some(value) = field_ids {
            // the partition columns are not written, unless WRITE_PARTITION_COLUMNS says so
            let written = match &options.partitioning {
                Some(partitioning) => partitioning.written_columns(bound.names.len()),
                None => (0..bound.names.len()).collect(),
            };
            let columns: Vec<(String, LogicalType)> = written
                .iter()
                .map(|&c| (bound.names[c].clone(), bound.types[c].clone()))
                .collect();
            options.writer.field_ids = if value.logical_type().id() == LogicalTypeId::Varchar
                && value.try_to_string()?.eq_ignore_ascii_case("auto")
            {
                let mut next = 0;
                generate_field_ids(&columns, &mut next)
            } else {
                parse_field_ids(&value, &columns, &mut std::collections::HashSet::new())?
            };
        }
        if let Some(value) = shredding {
            let written = match &options.partitioning {
                Some(partitioning) => partitioning.written_columns(bound.names.len()),
                None => (0..bound.names.len()).collect(),
            };
            let variants: Vec<&String> = written
                .iter()
                .filter(|&&c| bound.types[c].id() == LogicalTypeId::Variant)
                .map(|&c| &bound.names[c])
                .collect();
            options.writer.shredding = self.bind_shredding(&value, &variants)?;
        }
        options.rotation = partition.rotation(&path, "parquet", file_size_bytes, row_groups_per_file, per_thread)?;
        options.output = output.build(options.partitioning.is_some(), per_thread, ordered, None)?;
        options.writer.return_stats = options.output.return_stats;
        let (result_names, return_type) = copy_result(&options.output);
        let result_types = options.output.types();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::CopyToParquet {
                path,
                names: super::copy::unique_names(&bound.names),
                options,
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

impl Binder<'_> {
    /// The `SHREDDING` option: the type each VARIANT column is shredded on.
    fn bind_shredding(
        &mut self,
        value: &Value,
        variants: &[&String],
    ) -> Result<std::collections::HashMap<String, LogicalType>> {
        if value.logical_type().id() == LogicalTypeId::Varchar && value.try_to_string()?.eq_ignore_ascii_case("auto") {
            return Err(Error::not_implemented(
                "The 'auto' option is not yet implemented for 'shredding'",
            ));
        }
        let ValueData::Struct(fields) = value.data() else {
            return Err(binder_error(
                "SHREDDING value should be a STRUCT of column names to types, i.e: {col1: 'INTEGER[]', col2: 'BOOLEAN'}",
            ));
        };
        if value.logical_type().id() != LogicalTypeId::Struct || value.is_null() {
            return Err(binder_error(
                "SHREDDING value should be a STRUCT of column names to types, i.e: {col1: 'INTEGER[]', col2: 'BOOLEAN'}",
            ));
        }
        let mut result = std::collections::HashMap::new();
        for ((name, _), field) in value.logical_type().struct_children().iter().zip(fields) {
            let Some(column) = variants.iter().find(|c| c.eq_ignore_ascii_case(name)) else {
                if variants.is_empty() {
                    return Err(binder_error(format!(
                        "VARIANT by name \"{name}\" specified in SHREDDING not found. There are no VARIANT columns present."
                    )));
                }
                let names = variants.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
                return Err(binder_error(format!(
                    "VARIANT by name \"{name}\" specified in SHREDDING not found. Consider using WRITE_PARTITION_COLUMNS \
                     if this column is a partition column. Available names of VARIANT columns: [{names}]"
                )));
            };
            if field.logical_type().id() != LogicalTypeId::Varchar {
                return Err(binder_error(
                    "SHREDDING value should be of type VARCHAR, a stringified type to use for the column",
                ));
            }
            // a NULL, or the type NULL, writes the column without shredding
            let disabled = LogicalType::new(LogicalTypeId::Variant);
            if field.is_null() {
                result.insert(column.to_lowercase(), disabled);
                continue;
            }
            let text = field.try_to_string()?;
            let cannot = || {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Value \"{text}\" can not be converted to a DuckDB Type."),
                )
            };
            let ty = if text.trim().eq_ignore_ascii_case("null") {
                disabled
            } else {
                let ty = self.csv_type(&text).map_err(|error| match error.kind {
                    ErrorKind::Catalog => error,
                    _ => cannot(),
                })?;
                if contains_any(&ty) {
                    return Err(cannot());
                }
                ty
            };
            check_shredding_type(&ty)?;
            result.insert(column.to_lowercase(), ty);
        }
        Ok(result)
    }
}

fn contains_any(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Any
        || ty.child_type().is_some_and(contains_any)
        || ty.struct_children().iter().any(|(_, child)| contains_any(child))
}

/// `ConvertShreddingTypeRecursive`: the types a VARIANT can be shredded on in Parquet.
fn check_shredding_type(ty: &LogicalType) -> Result<()> {
    use LogicalTypeId as T;
    match ty.id() {
        T::Variant => Ok(()),
        T::Struct => ty
            .struct_children()
            .iter()
            .try_for_each(|(_, child)| check_shredding_type(child)),
        T::List => check_shredding_type(ty.child_type().expect("a LIST has a child")),
        _ if ty.is_nested() => Err(binder_error(format!(
            "VARIANT can only be shredded on LIST/STRUCT/ANY/non-nested type, not {ty}"
        ))),
        T::Bit | T::Bignum => Err(binder_error(format!(
            "VARIANT cannot be shredded on type {ty} in Parquet"
        ))),
        _ if ty.alias().is_some() => Err(binder_error(format!(
            "VARIANT cannot be shredded on type {ty} in Parquet"
        ))),
        _ => Ok(()),
    }
}

/// `LogicalType::GetNamedChildTypes`: the children of a nested type as Parquet names them.
fn named_children(ty: &LogicalType) -> Vec<(String, LogicalType)> {
    match ty.id() {
        LogicalTypeId::List | LogicalTypeId::Array => {
            vec![(
                "element".to_string(),
                ty.child_type().cloned().unwrap_or(LogicalType::INTEGER),
            )]
        }
        LogicalTypeId::Map => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::INTEGER);
            entry.struct_children().to_vec()
        }
        LogicalTypeId::Struct => ty.struct_children().to_vec(),
        LogicalTypeId::Tuple => ty
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
            .collect(),
        _ => Vec::new(),
    }
}

/// `FieldID::GenerateFieldIDs`: `FIELD_IDS 'auto'` numbers the columns depth first.
fn generate_field_ids(columns: &[(String, LogicalType)], next: &mut i32) -> FieldIds {
    let mut ids = FieldIds::default();
    for (name, ty) in columns {
        let id = *next;
        *next += 1;
        let children = generate_field_ids(&named_children(ty), next);
        ids.0.insert(name.to_lowercase(), (Some(id), children));
    }
    ids
}

/// `FieldID::GetFieldIDs`: `{col: 42, nested: {__duckdb_field_id: 43, child: 44}}`.
fn parse_field_ids(
    value: &Value,
    columns: &[(String, LogicalType)],
    unique: &mut std::collections::HashSet<i32>,
) -> Result<FieldIds> {
    const DUCKDB_FIELD_ID: &str = "__duckdb_field_id";
    if columns.iter().any(|(name, _)| name == DUCKDB_FIELD_ID) {
        return Err(binder_error(format!(
            "Cannot have column named \"{DUCKDB_FIELD_ID}\" with FIELD_IDS"
        )));
    }
    let ValueData::Struct(items) = value.data() else {
        return Err(binder_error(format!(
            "Expected FIELD_IDS to be a STRUCT, e.g., {{col1: 42, col2: {{{DUCKDB_FIELD_ID}: 43, nested_col: 44}}, \
             col3: 44}}"
        )));
    };
    let mut ids = FieldIds::default();
    for ((key, _), item) in value.logical_type().struct_children().iter().zip(items) {
        let name = key.to_lowercase();
        if name == DUCKDB_FIELD_ID {
            continue;
        }
        let Some((_, column_type)) = columns.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)) else {
            let names: Vec<String> = columns.iter().map(|(n, _)| n.to_lowercase()).collect();
            return Err(binder_error(format!(
                "Column name \"{name}\" specified in FIELD_IDS not found. Consider using WRITE_PARTITION_COLUMNS if \
                 this column is a partition column. Available column names: [{}]",
                names.join(", ")
            )));
        };
        let (id_value, nested) = match item.data() {
            ValueData::Struct(parts) => {
                let mut id_value = None;
                let mut nested = false;
                for ((part_name, _), part) in item.logical_type().struct_children().iter().zip(parts) {
                    if part_name == DUCKDB_FIELD_ID {
                        id_value = Some(part);
                    } else {
                        nested = true;
                    }
                }
                (id_value, nested)
            }
            _ => (Some(item), false),
        };
        let mut field_id = None;
        if let Some(id_value) = id_value {
            // `DefaultCastAs(INTEGER)`, which fails on text that isn't a number
            let id = match id_value.as_i128() {
                Some(id) => Some(id),
                None if id_value.is_null() => None,
                None => {
                    let text = id_value.try_to_string()?;
                    match text.trim().parse::<i128>() {
                        Ok(id) => Some(id),
                        Err(_) => {
                            return Err(Error::new(
                                ErrorKind::InvalidInput,
                                format!("Could not convert string '{text}' to INT32"),
                            ));
                        }
                    }
                }
            }
            .filter(|id| (0..=i128::from(i32::MAX)).contains(id));
            let Some(id) = id else {
                return Err(binder_error(format!(
                    "Invalid field_id {} found in FIELD_IDS: field ids must be non-negative integers",
                    id_value.try_to_string()?
                )));
            };
            if !unique.insert(id as i32) {
                return Err(binder_error(format!("Duplicate field_id {id} found in FIELD_IDS")));
            }
            field_id = Some(id as i32);
        }
        let mut children = FieldIds::default();
        if nested {
            let child_columns = named_children(column_type);
            if child_columns.is_empty() {
                return Err(binder_error(format!(
                    "Column \"{name}\" with type \"{}\" cannot have a nested FIELD_IDS specification",
                    column_type.id().name()
                )));
            }
            children = parse_field_ids(item, &child_columns, unique)?;
        }
        ids.0.insert(name, (field_id, children));
    }
    Ok(ids)
}

/// `ParquetCopyOptions`.
const PARQUET_COPY_OPTIONS: &[&str] = &[
    "compression",
    "codec",
    "field_ids",
    "kv_metadata",
    "encryption_config",
    "dictionary_size_limit",
    "string_dictionary_page_size_limit",
    "data_page_size_limit",
    "bloom_filter_false_positive_ratio",
    "debug_use_openssl",
    "write_bloom_filter",
    "bloom_filter_fallback_max_bytes",
    "write_page_index",
    "compression_level",
    "parquet_version",
    "binary_as_string",
    "file_row_number",
    "can_have_nan",
    "int96_as",
    "geoparquet_version",
    "shredding",
    "write_timestamp_as_int96",
    "timestamp_is_adjusted_to_utc",
    "row_group_size",
    "chunk_size",
    "row_group_size_bytes",
    "row_groups_per_file",
    "dictionary_compression_ratio_threshold",
];
