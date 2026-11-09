// Ported from duckdb/extension/parquet/parquet_extension.cpp @ rust-port-baseline
//
//! `read_parquet` and COPY with FORMAT PARQUET.

use super::copy::{
    OutputOptionsBuilder, PartitionOptions, copy_result, hive_columns, is_true, parse_bytes_argument, string_literal,
    strings,
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
    pub(super) fn bind_read_parquet(&mut self, call: &FunctionExpr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        let mut paths = None;
        let (mut binary_as_string, mut union_by_name, mut file_row_number) = (false, false, false);
        let mut filename = None;
        let mut hive_partitioning = None;
        let mut int96_as = crate::execution::parquet::Int96As::Timestamp;
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
            match option.as_str() {
                "binary_as_string" => binary_as_string = is_true(&value, &option)?,
                "union_by_name" => union_by_name = is_true(&value, &option)?,
                "file_row_number" => file_row_number = is_true(&value, &option)?,
                "hive_partitioning" => hive_partitioning = Some(is_true(&value, &option)?),
                "int96_as" => {
                    use crate::execution::parquet::Int96As;
                    int96_as = match value.try_to_string()?.to_lowercase().as_str() {
                        "timestamp" => Int96As::Timestamp,
                        "timestamp_ns" => Int96As::TimestampNs,
                        "struct" => Int96As::Struct,
                        other => {
                            return Err(binder_error(format!(
                                "int96_as must be 'timestamp', 'timestamp_ns' or 'struct', not '{other}'"
                            )));
                        }
                    };
                }
                "hive_types_autocast" | "explicit_cardinality" | "can_have_nan" => {}
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
                    filename = match value.data() {
                        ValueData::Bool(true) => Some("filename".to_string()),
                        ValueData::Bool(false) => None,
                        _ => Some(value.try_to_string()?),
                    };
                }
                _ => return Err(crate::unported(format!("the {name} option {option}"))),
            }
        }
        let patterns = paths.ok_or_else(|| binder_error(format!("{name} requires a file name")))?;
        let mut files = Vec::new();
        for pattern in &patterns {
            let found = file_system::glob(pattern);
            if found.is_empty() {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            files.extend(found);
        }
        if files.is_empty() {
            return Err(binder_error(format!("{name} needs at least one file to read")));
        }
        let (mut names, mut types) = file_schema(&files[0], binary_as_string, int96_as)?;
        if union_by_name {
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
        let hive = hive_columns(&files, hive_partitioning, &names)?;
        for (key, ty) in &hive {
            output_names.push(key.clone());
            output_types.push(ty.clone());
        }
        let alias = if table_ref.alias.is_empty() {
            name
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
        let scan = ParquetScanData {
            files,
            names,
            types,
            binary_as_string,
            int96_as,
            union_by_name,
            file_row_number,
            filename: filename.is_some(),
            hive,
            virtual_columns,
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
            let found = file_system::glob(&pattern);
            if found.is_empty() {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            files.extend(found);
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
        let select = parse_select(
            &format!("SELECT * FROM read_parquet({})", string_literal(path)),
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
                "compression" | "codec" => compression = Some(value.try_to_string()?),
                "compression_level" => level = Some(number()? as i32),
                "row_group_size" | "chunk_size" => writer.row_group_size = number()?.max(1) as usize,
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
                _ => return Err(crate::unported(format!("the COPY option {name} for Parquet"))),
            }
        }
        if let Some(name) = compression {
            writer.codec = parse_compression(&name, level)?;
        }
        // `DEFAULT_DATA_PAGE_SIZE_LIMIT`: with the page index, pages of 1 MiB; without, one per column chunk
        writer.data_page_size_limit = page_size_limit.unwrap_or(if writer.write_page_index {
            1_048_576
        } else {
            MAX_UNCOMPRESSED_PAGE_SIZE
        });
        if let Some(value) = field_ids {
            let columns: Vec<(String, LogicalType)> =
                bound.names.iter().cloned().zip(bound.types.iter().cloned()).collect();
            options.writer.field_ids = if value.logical_type().id() == LogicalTypeId::Varchar
                && value.try_to_string()?.eq_ignore_ascii_case("auto")
            {
                let mut next = 0;
                generate_field_ids(&columns, &mut next)
            } else {
                parse_field_ids(&value, &columns, &mut std::collections::HashSet::new())?
            };
        }
        options.partitioning = partition.bind(&path, &bound.names, "parquet")?;
        options.rotation = partition.rotation(&path, "parquet", file_size_bytes, row_groups_per_file, per_thread)?;
        options.output = output.build(options.partitioning.is_some(), per_thread, ordered)?;
        let (result_names, return_type) = copy_result(&options.output);
        let result_types = options.output.types();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::CopyToParquet {
                path,
                names: bound.names.clone(),
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
