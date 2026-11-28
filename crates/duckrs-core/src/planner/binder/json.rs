// Ported from duckdb/extension/json/json_functions/read_json.cpp and copy_json.cpp @ rust-port-baseline
//
//! `read_json`, `read_ndjson`, `read_json_objects` and their `_auto` forms.

use super::copy::{is_true, strings};
use super::table_functions::{named_argument, parse_select};
use super::{Binder, Binding, binder_error};
use crate::execution::file_system::{self, Compression, read_file};
use crate::execution::json_scan::{
    Formats, JsonFormat, JsonScanData, infer_options, read_records, record_error, refine_type,
};
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
        let single = name == "read_single_json_file";
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
        let mut map_inference = true;
        let mut formats = Formats {
            missing_keys_are_null: true,
            map_threshold: Some(200),
            ..Formats::default()
        };
        let mut compression = Compression::Auto;
        let mut filename = None;
        let mut hive_options = super::copy::HiveOptions::default();
        let mut union_by_name = false;
        // `maximum_sample_files`: the files `DetectStructure` samples
        let mut maximum_sample_files = 32usize;
        // `geojson`: left to it, a file named `.geojson` or `.geojsonl` is GeoJSON
        let mut geojson: Option<bool> = None;
        // `allow_empty`: a pattern no file matches reads no rows
        let mut allow_empty = false;
        for argument in &args {
            let value = self.constant_argument(&argument.expr)?;
            if argument.name.is_empty() {
                if paths.is_some() {
                    return Err(binder_error(format!("{name} takes one positional parameter")));
                }
                if single && value.is_null() {
                    return Err(binder_error("read_single_json_file requires a non-NULL file name"));
                }
                if value.is_null() {
                    return Err(binder_error(format!("{name} cannot take NULL list as parameter")));
                }
                // `ReadSingleJSONFileBind`: one file, by its name
                if single && value.logical_type().id() != LogicalTypeId::Varchar {
                    return Err(binder_error(format!(
                        "read_single_json_file reads a single file - given as a VARCHAR path or as a file struct, not \
                         as {}",
                        value.logical_type()
                    )));
                }
                paths = Some(strings(&value)?);
                continue;
            }
            let option = argument.name.to_lowercase();
            // `AddReadJSONParameters` and `AddAutoDetectParameters`: what read_json_objects does not take
            if objects
                && matches!(
                    option.as_str(),
                    "columns"
                        | "auto_detect"
                        | "geojson"
                        | "sample_size"
                        | "dateformat"
                        | "date_format"
                        | "timestampformat"
                        | "timestamp_format"
                        | "records"
                        | "array"
                        | "maximum_sample_files"
                        | "case_insensitive_keys"
                        | "maximum_depth"
                        | "field_appearance_threshold"
                        | "map_inference_threshold"
                        | "convert_strings_to_integers"
                )
            {
                return Err(binder_error(format!(
                    "Invalid named parameter \"{}\" for function {name}",
                    argument.name
                )));
            }
            let number = || -> Result<i64> {
                value
                    .try_to_string()?
                    .parse()
                    .map_err(|_| binder_error(format!("{option} must be an integer")))
            };
            match option.as_str() {
                "format" | "json_format" => format = format_of(&value.try_to_string()?)?,
                // records is tri-state, and a boolean is one of its two others
                "records" => {
                    let text = match value.logical_type().id() {
                        LogicalTypeId::Boolean => value.try_to_string()?,
                        LogicalTypeId::Varchar => value.try_to_string()?,
                        _ => {
                            return Err(binder_error(format!(
                                "read_json \"records\" parameter must be a string ('auto', 'true' or 'false') or a \
                                 boolean, not {}",
                                value.logical_type()
                            )));
                        }
                    };
                    records = match text.as_str() {
                        "auto" => None,
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => {
                            return Err(binder_error(
                                "read_json requires \"records\" to be one of ['auto', 'true', 'false'].",
                            ));
                        }
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
                // -1 (`DConstants::INVALID_INDEX`): no objects become MAPs
                "map_inference_threshold" => {
                    let threshold = number()?;
                    map_inference = threshold != -1;
                    formats.map_threshold = usize::try_from(threshold).ok();
                }
                "maximum_sample_files" => {
                    if value.is_null() {
                        return Err(binder_error(
                            "read_json \"maximum_sample_files\" parameter cannot be NULL",
                        ));
                    }
                    let files = number()?;
                    if files == 0 || files < -1 {
                        return Err(binder_error(
                            "read_json \"maximum_sample_files\" parameter must be positive, or -1 to remove the limit \
                             on the number of files used to sample \"sample_size\" rows.",
                        ));
                    }
                    maximum_sample_files = if files < 0 { usize::MAX } else { files as usize };
                }
                "maximum_object_size" | "field_appearance_threshold" | "convert_strings_to_integers" => {}
                "union_by_name" => union_by_name = is_true(&value, &option)?,
                "case_insensitive_keys" => formats.case_insensitive = is_true(&value, &option)?,
                "geojson" => geojson = Some(is_true(&value, &option)?),
                "allow_empty" => allow_empty = is_true(&value, &option)?,
                "hive_partitioning" | "hive_types" | "hive_type" | "hive_types_autocast" | "hive_type_autocast" => {
                    self.take_hive_option(&mut hive_options, &option, &value)?;
                }
                "filename" => {
                    filename = super::copy::filename_option(&value, &option)?;
                }
                // `TableFunctionBinder::BindNamedParameters`: what C++ has is not ported yet, the rest is no option
                _ if READ_JSON_OPTIONS.contains(&option.as_str()) => {
                    return Err(crate::unported(format!("the {name} option {option}")));
                }
                _ => {
                    return Err(binder_error(format!(
                        "Invalid named parameter \"{}\" for function {name}",
                        argument.name
                    )));
                }
            }
        }
        let patterns = paths.ok_or_else(|| binder_error(format!("{name} requires a file name")))?;
        let mut files = Vec::new();
        for pattern in &patterns {
            self.check_file_access(pattern)?;
            let found = if single {
                // the file itself, never a pattern
                if std::path::Path::new(pattern).is_file() {
                    vec![pattern.clone()]
                } else {
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!("Cannot open file \"{pattern}\": No such file or directory"),
                    ));
                }
            } else {
                file_system::glob_or_directory(pattern, "json")
            };
            if found.is_empty() && !allow_empty {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("No files found that match the pattern \"{pattern}\""),
                ));
            }
            files.extend(found);
        }
        // records='false' reads the values as they are, whatever the file is called
        let geojson = !objects
            && geojson.unwrap_or_else(|| {
                records != Some(false) && files.iter().any(|f| crate::execution::json_scan::is_geojson_file(f))
            });
        let auto_detect = auto_detect.unwrap_or(columns.is_none());
        if !auto_detect && columns.is_none() && !objects {
            return Err(binder_error(
                "When auto_detect=false, read_json requires columns to be specified through the \"columns\" parameter.",
            ));
        }
        let mut cardinality_per_file = None;
        let (names, types, use_records) = if objects {
            (vec!["json".to_string()], vec![json_type()], false)
        } else if let Some(columns) = columns {
            // `JSONScan::AutoDetect` still samples the first file when the format or `records` is left to it
            let mut sampled_objects = true;
            if (format == JsonFormat::Auto || records.is_none())
                && let Some(path) = files.first()
            {
                let data = read_file(path, compression)?;
                let (found, detected) = read_records(&data, format);
                let size = std::fs::metadata(path).map_or(data.len() as u64, |m| m.len());
                cardinality_per_file =
                    crate::execution::json_scan::estimate_per_file(&data, &found, detected, size, sample_size);
                sampled_objects = found
                    .iter()
                    .take(sample_size)
                    .any(|record| matches!(record, Ok((Json::Object(_), _))));
            }
            // records left to it, one column and no objects sampled: the values themselves are the column
            let use_records = records.unwrap_or(columns.len() != 1 || sampled_objects);
            let (names, types): (Vec<_>, Vec<_>) = columns.into_iter().unzip();
            (names, types, use_records)
        } else {
            let mut sampled: Vec<Json> = Vec::new();
            // `DetectStructure`: up to `sample_size` records of each of the first files
            for (at, path) in files.iter().enumerate().take(maximum_sample_files.max(1)) {
                let mut taken = 0;
                let data = read_file(path, compression)?;
                let (found, detected) = read_records(&data, format);
                if at == 0 {
                    let size = std::fs::metadata(path).map_or(data.len() as u64, |m| m.len());
                    cardinality_per_file =
                        crate::execution::json_scan::estimate_per_file(&data, &found, detected, size, sample_size);
                }
                let found = if geojson {
                    crate::execution::json_scan::geojson_records(found)
                } else {
                    found
                };
                for record in found {
                    match record {
                        Ok((record, _)) => sampled.push(record),
                        Err(_) if ignore_errors => {}
                        Err(error) => return Err(record_error(path, &data, &error)),
                    }
                    taken += 1;
                    if taken >= sample_size {
                        break;
                    }
                }
            }
            // the schema of the sampled files is combined either way; union_by_name changes how files are read
            let _ = union_by_name;
            let references: Vec<&Json> = sampled.iter().collect();
            let structure = infer_options(&references, formats.case_insensitive, geojson);
            let ty = structure.logical_type_with(maximum_depth, map_inference);
            let ty = refine_type(ty, &references, &formats);
            if formats.date.is_none() {
                formats.date = formats.detected_date.get().map(str::to_string);
            }
            if formats.timestamp.is_none() {
                formats.timestamp = formats.detected_timestamp.get().map(str::to_string);
            }
            let is_object = ty.id() == LogicalTypeId::Struct;
            let use_records = records.unwrap_or(is_object);
            if use_records && !is_object {
                if records == Some(true) {
                    return Err(binder_error(
                        "json_read expected records, but got non-record JSON instead.\n Try setting records='auto' or \
                         records='false'.",
                    ));
                }
                (
                    vec!["json".to_string()],
                    vec![remove_duplicate_struct_keys(&ty, ignore_errors)?],
                    false,
                )
            } else if use_records {
                let (names, types): (Vec<String>, Vec<LogicalType>) = ty.struct_children().iter().cloned().unzip();
                let types = types
                    .iter()
                    .map(|ty| remove_duplicate_struct_keys(ty, ignore_errors))
                    .collect::<Result<_>>()?;
                (names, types, true)
            } else {
                (
                    vec!["json".to_string()],
                    vec![remove_duplicate_struct_keys(&ty, ignore_errors)?],
                    false,
                )
            }
        };
        // `BindTableFunction`: names of keys that differ in case only are made unique; the scan
        // still reads each key by its own name
        super::copy::check_generated_columns(&names, filename.as_deref(), false)?;
        let mut output_names = super::copy::unique_names(&names);
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
        let name_for_explain = name.clone();
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
            catalog: None,
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
            geojson,
            names,
            types,
            ignore_errors,
            auto_detect,
            case_insensitive: formats.case_insensitive,
            date_format: formats.date,
            timestamp_format: formats.timestamp,
            filename: filename.is_some(),
            hive,
            virtual_columns,
            function_name: name_for_explain,
            projection: None,
            column_order: self.column_order(table_index),
            file_filters: String::new(),
            total_files: None,
            cardinality_per_file,
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

/// `RemoveDuplicateStructKeys`: of fields that are the same name in another case, the first; the
/// others are an error unless errors are ignored.
fn remove_duplicate_struct_keys(ty: &LogicalType, ignore_errors: bool) -> Result<LogicalType> {
    Ok(match ty.id() {
        LogicalTypeId::Struct => {
            let mut fields: Vec<(String, LogicalType)> = Vec::new();
            for (name, child) in ty.struct_children() {
                if fields.iter().any(|(seen, _)| seen.eq_ignore_ascii_case(name)) {
                    if ignore_errors {
                        continue;
                    }
                    return Err(Error::not_implemented(format!(
                        "Duplicate name {name} in struct auto-detected in JSON, try ignore_errors=true"
                    )));
                }
                fields.push((name.clone(), remove_duplicate_struct_keys(child, ignore_errors)?));
            }
            LogicalType::struct_type(fields)
        }
        LogicalTypeId::Map => {
            let entry = ty.child_type().map(LogicalType::struct_children).unwrap_or_default();
            LogicalType::map(
                remove_duplicate_struct_keys(&entry[0].1, ignore_errors)?,
                remove_duplicate_struct_keys(&entry[1].1, ignore_errors)?,
            )
        }
        LogicalTypeId::List => LogicalType::list(remove_duplicate_struct_keys(
            ty.child_type().expect("a LIST has a child"),
            ignore_errors,
        )?),
        _ => ty.clone(),
    })
}

/// The named parameters of C++'s `read_json` (`AddReadJSONParameters`, `AddAutoDetectParameters`,
/// `TableFunctionDefaults` and `MultiFileReader::AddParameters`).
const READ_JSON_OPTIONS: &[&str] = &[
    "columns",
    "auto_detect",
    "geojson",
    "sample_size",
    "dateformat",
    "date_format",
    "timestampformat",
    "timestamp_format",
    "records",
    "array",
    "maximum_sample_files",
    "case_insensitive_keys",
    "maximum_depth",
    "field_appearance_threshold",
    "map_inference_threshold",
    "convert_strings_to_integers",
    "maximum_object_size",
    "ignore_errors",
    "format",
    "compression",
    "filename",
    "hive_partitioning",
    "union_by_name",
    "hive_types",
    "hive_types_autocast",
    "schema",
    "file_row_number",
    "column_statistics",
    "allow_empty",
];
