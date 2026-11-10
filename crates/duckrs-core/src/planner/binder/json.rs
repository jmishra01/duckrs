// Ported from duckdb/extension/json/json_functions/read_json.cpp and copy_json.cpp @ rust-port-baseline
//
//! `read_json`, `read_ndjson`, `read_json_objects` and their `_auto` forms.

use super::copy::{is_true, strings};
use super::table_functions::{named_argument, parse_select};
use super::{Binder, Binding, binder_error};
use crate::execution::file_system::{self, Compression, read_file};
use crate::execution::json_scan::{Formats, JsonFormat, JsonScanData, infer, read_records, record_error, refine_type};
use crate::function::json::{Json, TABLE_IN_OUT_COLUMNS, json_type};
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::ValueData;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{FunctionExpr, TableRef};
use std::sync::Arc;

fn format_of(text: &str) -> Result<JsonFormat> {
    match text.to_lowercase().as_str() {
        "auto" => Ok(JsonFormat::Auto),
        "unstructured" => Ok(JsonFormat::Unstructured),
        "newline_delimited" | "nd" => Ok(JsonFormat::NewlineDelimited),
        "array" => Ok(JsonFormat::Array),
        _ => Err(binder_error(
            "format must be one of ['auto', 'unstructured', 'newline_delimited', 'array']",
        )),
    }
}

impl Binder<'_> {
    /// `ReadJSONBind`.
    pub(super) fn bind_read_json(&mut self, call: &FunctionExpr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let objects = name.contains("objects");
        let mut format = if name.contains("ndjson") {
            JsonFormat::NewlineDelimited
        } else {
            JsonFormat::Auto
        };
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        let mut paths = None;
        let mut records: Option<bool> = None;
        let mut columns: Option<Vec<(String, LogicalType)>> = None;
        let (mut auto_detect, mut ignore_errors) = (None, false);
        let (mut sample_size, mut maximum_depth) = (20480usize, -1i64);
        let mut formats = Formats {
            missing_keys_are_null: true,
            map_threshold: Some(200),
            ..Formats::default()
        };
        let mut compression = Compression::Auto;
        let mut filename = None;
        let mut hive_options = super::copy::HiveOptions::default();
        let mut union_by_name = false;
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
            let number = || -> Result<i64> {
                value
                    .try_to_string()?
                    .parse()
                    .map_err(|_| binder_error(format!("{option} must be an integer")))
            };
            match option.as_str() {
                "format" | "json_format" => format = format_of(&value.try_to_string()?)?,
                "records" => {
                    records = match value.try_to_string()?.to_lowercase().as_str() {
                        "auto" => None,
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => return Err(binder_error("\"records\" must be one of ['auto', 'true', 'false']")),
                    };
                }
                "lines" => {
                    if is_true(&value, &option)? {
                        format = JsonFormat::NewlineDelimited;
                    }
                }
                "array" => {
                    format = if is_true(&value, &option)? {
                        JsonFormat::Array
                    } else {
                        JsonFormat::NewlineDelimited
                    };
                }
                "columns" => {
                    if value.logical_type().id() != LogicalTypeId::Struct {
                        return Err(binder_error(
                            "read_json \"columns\" parameter requires a struct as input.",
                        ));
                    }
                    let ValueData::Struct(fields) = value.data() else {
                        return Err(binder_error(
                            "read_json \"columns\" parameter requires a struct as input.",
                        ));
                    };
                    let keys = value.logical_type().struct_children().to_vec();
                    let mut parsed = Vec::with_capacity(keys.len());
                    for ((key, _), field) in keys.iter().zip(fields) {
                        if field.is_null() || field.logical_type().id() != LogicalTypeId::Varchar {
                            return Err(binder_error(
                                "read_json \"columns\" parameter type specification must be VARCHAR.",
                            ));
                        }
                        parsed.push((key.clone(), self.csv_type(&field.try_to_string()?)?));
                    }
                    columns = Some(parsed);
                }
                "auto_detect" => auto_detect = Some(is_true(&value, &option)?),
                "ignore_errors" => ignore_errors = is_true(&value, &option)?,
                "sample_size" => {
                    let size = number()?;
                    if size < 1 && size != -1 {
                        return Err(binder_error("Invalid sample_size: must be -1 or a positive number"));
                    }
                    sample_size = if size < 0 { usize::MAX } else { size as usize };
                }
                "maximum_depth" => maximum_depth = number()?,
                "dateformat" | "date_format" => formats.date = Some(value.try_to_string()?),
                "timestampformat" | "timestamp_format" => formats.timestamp = Some(value.try_to_string()?),
                "compression" => compression = Compression::parse(&value.try_to_string()?)?,
                "maximum_object_size"
                | "map_inference_threshold"
                | "field_appearance_threshold"
                | "maximum_sample_files"
                | "convert_strings_to_integers" => {}
                "union_by_name" => union_by_name = is_true(&value, &option)?,
                "hive_partitioning" | "hive_types" | "hive_type" | "hive_types_autocast" | "hive_type_autocast" => {
                    self.take_hive_option(&mut hive_options, &option, &value)?;
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
            let found = file_system::glob_or_directory(pattern, "json");
            if found.is_empty() {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            files.extend(found);
        }
        let auto_detect = auto_detect.unwrap_or(columns.is_none());
        if !auto_detect && columns.is_none() && !objects {
            return Err(binder_error(
                "read_json \"columns\" parameter is required when auto_detect is false",
            ));
        }
        let (names, types, use_records) = if objects {
            (vec!["json".to_string()], vec![json_type()], false)
        } else if let Some(columns) = columns {
            let (names, types): (Vec<_>, Vec<_>) = columns.into_iter().unzip();
            (names, types, records.unwrap_or(true))
        } else {
            let mut sampled: Vec<Json> = Vec::new();
            for path in &files {
                let data = read_file(path, compression)?;
                let (found, _) = read_records(&data, format);
                for record in found {
                    match record {
                        Ok(record) => sampled.push(record),
                        Err(_) if ignore_errors => {}
                        Err(error) => return Err(record_error(path, &data, &error)),
                    }
                    if sampled.len() >= sample_size {
                        break;
                    }
                }
                if sampled.len() >= sample_size || !(union_by_name || sampled.is_empty()) {
                    break;
                }
            }
            let references: Vec<&Json> = sampled.iter().collect();
            let structure = infer(&references);
            let ty = structure.logical_type(maximum_depth);
            let ty = refine_type(ty, &references, &formats);
            let is_object = ty.id() == LogicalTypeId::Struct;
            let use_records = records.unwrap_or(is_object);
            if use_records && !is_object {
                if records == Some(true) {
                    return Err(binder_error(
                        "read_json requires records of objects to be read with records=true",
                    ));
                }
                (vec!["json".to_string()], vec![ty], false)
            } else if use_records {
                let (names, types) = ty.struct_children().iter().cloned().unzip();
                (names, types, true)
            } else {
                (vec!["json".to_string()], vec![ty], false)
            }
        };
        let mut output_names = names.clone();
        let mut output_types = types.clone();
        if let Some(column) = &filename {
            output_names.push(column.clone());
            output_types.push(LogicalType::VARCHAR);
        }
        let hive = super::copy::hive_columns_with(&files, &hive_options, &names)?;
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
        let scan = JsonScanData {
            files,
            format,
            compression,
            records: use_records,
            objects,
            names,
            types,
            ignore_errors,
            case_insensitive: formats.case_insensitive,
            date_format: formats.date,
            timestamp_format: formats.timestamp,
            filename: filename.is_some(),
            hive,
            virtual_columns,
        };
        Ok(LogicalOperator::new(
            LogicalOperatorKind::JsonScan {
                table_index,
                scan: Arc::new(scan),
            },
            vec![],
        ))
    }
}

impl Binder<'_> {
    /// `json_each` and `json_tree` (`JSONTableInOutBind`): the unnested rows of `__json_each` or `__json_tree`, so
    /// that their arguments can be columns of a LATERAL join, with the hidden columns `json`, `root` and `rowid`.
    pub(super) fn bind_json_table_in_out(
        &mut self,
        call: &FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let name = call.name.name.to_lowercase();
        let args: Vec<_> = call.args.iter().map(named_argument).collect();
        if args.iter().any(|argument| !argument.name.is_empty()) || !(1..=2).contains(&args.len()) {
            return Err(binder_error(format!(
                "No function matches the given name and argument types for {name}"
            )));
        }
        let arguments: Vec<String> = args.iter().map(|argument| argument.expr.to_string()).collect();
        let columns: Vec<String> = TABLE_IN_OUT_COLUMNS
            .iter()
            .map(|column| format!("__row['{column}'] AS \"{column}\""))
            .collect();
        let select = parse_select(
            &format!(
                "SELECT {} FROM (SELECT unnest(__{name}({})) AS __row)",
                columns.join(", "),
                arguments.join(", ")
            ),
            "Expected a single SELECT statement",
        )?;
        let plan = self.bind_rewritten(&select, table_ref, &name)?;
        let binding = self.bind_context.bindings.last_mut().expect("just bound");
        let visible = TABLE_IN_OUT_COLUMNS.len() - 3;
        binding.virtual_columns = vec![
            ("json".to_string(), json_type(), visible),
            ("root".to_string(), LogicalType::VARCHAR, visible + 1),
        ];
        binding.row_id = Some(visible + 2);
        binding.names.truncate(visible);
        binding.types.truncate(visible);
        Ok(plan)
    }
}
