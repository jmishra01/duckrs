// Ported from duckdb/src/main/config.cpp (option lookup, ResolveOrder, ResolveNullOrder),
// duckdb/src/main/settings/custom_settings.cpp (DefaultNullOrderSetting, DefaultOrderSetting),
// duckdb/src/execution/operator/helper/physical_{set,reset}.cpp and
// duckdb/src/catalog/catalog.cpp (UnrecognizedConfigurationError) @ rust-port-baseline
//
//! Configuration options (`SET`, `RESET`, `PRAGMA x = y`). The settings themselves are generated
//! from DuckDB's `settings.json` ([`super::settings_table`]); values live per database (global)
//! and per connection (session), and a session value wins.
//!
//! Most settings tune performance, storage, extensions or logging, which duckrs either does
//! not have yet or does not need, so their values are stored and that is faithful. The few
//! that change what a query returns are either honored (the default sort order, the
//! expression depth limit) or listed in [`UNHONORED`]: setting one of those to anything but
//! its default fails as not yet ported, so a test never sees a silently different result.

use super::settings_table::SETTINGS;
use crate::execution::file_system::{FileAccess, sanitize_allowed_path};
use crate::function::cast::{CastMode, cast_vector};
use duckrs_common::string_util::{candidates_error_message, candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::vector::Vector;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::SetScope;
use std::collections::HashMap;

/// Where a setting may be set (`SettingScopeTarget`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingScope {
    GlobalOnly,
    LocalOnly,
    GlobalDefault,
    LocalDefault,
}

/// A configuration option (`ConfigurationOption`), generated from `settings.json`.
#[derive(Debug)]
pub struct SettingDefinition {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// The SQL type a value is cast to (`InputType`).
    pub input_type: &'static str,
    pub scope: SettingScope,
    /// The default as text, for the generic settings that have one.
    pub default_value: Option<&'static str>,
    /// The enum a value must name (`EnumUtil::FromString`), if the setting is checked that way.
    pub enum_name: Option<&'static str>,
    pub enum_values: &'static [&'static str],
    pub description: &'static str,
    pub is_debug: bool,
    pub is_deprecated: bool,
}

/// Settings that change query results or how SQL is read, whose effect duckrs does not
/// implement yet.
const UNHONORED: &[&str] = &[
    "access_mode",
    "active_grammar_extensions",
    "allow_parser_override_extension",
    "current_dialect",
    "dialect_compatibility_mode",
    "disable_timestamptz_casts",
    "errors_as_json",
    "file_search_path",
    "home_directory",
    "integer_division",
    "legacy_disable_null_type",
    "lock_configuration",
    "old_implicit_casting",
    "order_by_non_integer_literal",
    "binary_as_string",
    "preserve_identifier_case",
    "regex_match_operator_semantics",
    "schema",
    "search_path",
    "table_function_identifier_conversion",
];

/// `config.AddExtensionOption` of the Parquet extension, which is compiled in.
/// The descriptions are those of `ParquetExtension::Load`.
static EXTENSION_SETTINGS: [SettingDefinition; 7] = [
    extension_setting(
        "binary_as_string",
        "BOOLEAN",
        Some("false"),
        "In Parquet files, interpret binary data as a string.",
    ),
    extension_setting(
        "disable_parquet_prefetching",
        "BOOLEAN",
        Some("false"),
        "Disable the prefetching mechanism in Parquet",
    ),
    extension_setting(
        "prefetch_all_parquet_files",
        "BOOLEAN",
        Some("false"),
        "(deprecated) Parquet files are now always prefetched, this setting has no effect",
    ),
    extension_setting(
        "parquet_prefetch_column_gap",
        "UBIGINT",
        None,
        "Byte gap under which Parquet prefetch I/O ranges are coalesced (NULL lets the cost model adapt it)",
    ),
    extension_setting(
        "parquet_metadata_cache",
        "BOOLEAN",
        Some("false"),
        "Cache Parquet metadata - useful when reading the same files multiple times",
    ),
    extension_setting(
        "parquet_use_page_index",
        "BOOLEAN",
        Some("true"),
        "Use the page index (ColumnIndex and OffsetIndex) of Parquet files to skip the pages that cannot contain rows matching the filters",
    ),
    extension_setting(
        "enable_geoparquet_conversion",
        "BOOLEAN",
        Some("true"),
        "Attempt to decode/encode geometry data in/as GeoParquet files if the spatial extension is present.",
    ),
];

const fn extension_setting(
    name: &'static str,
    input_type: &'static str,
    default_value: Option<&'static str>,
    description: &'static str,
) -> SettingDefinition {
    SettingDefinition {
        name,
        aliases: &[],
        input_type,
        scope: SettingScope::GlobalDefault,
        default_value,
        enum_name: None,
        enum_values: &[],
        description,
        is_debug: false,
        is_deprecated: false,
    }
}

/// `DBConfig::GetOptionByName`: a setting by its name or an alias, case-insensitively.
pub fn find_setting(name: &str) -> Option<(usize, &'static SettingDefinition)> {
    SETTINGS
        .iter()
        .chain(&EXTENSION_SETTINGS)
        .enumerate()
        .find(|(_, s)| s.name.eq_ignore_ascii_case(name) || s.aliases.iter().any(|a| a.eq_ignore_ascii_case(name)))
}

/// `Catalog::UnrecognizedConfigurationError`.
fn unrecognized(name: &str) -> Error {
    let mut names: Vec<String> = SETTINGS
        .iter()
        .chain(&EXTENSION_SETTINGS)
        .map(|s| s.name.to_string())
        .collect();
    names.extend(SETTINGS.iter().flat_map(|s| s.aliases.iter().map(|a| a.to_string())));
    Error::new(
        ErrorKind::Catalog,
        format!(
            "unrecognized configuration parameter {name}\n{}",
            candidates_error_message(&names, name, "Did you mean")
        ),
    )
}

/// `DBConfig::ParseLogicalType` for the input types of the settings duckrs can store.
fn input_type(setting: &SettingDefinition) -> Result<LogicalType> {
    Ok(match setting.input_type {
        "VARCHAR" => LogicalType::VARCHAR,
        "BOOLEAN" => LogicalType::BOOLEAN,
        "UBIGINT" => LogicalType::UBIGINT,
        "BIGINT" => LogicalType::BIGINT,
        "DOUBLE" => LogicalType::DOUBLE,
        "VARCHAR[]" => LogicalType::list(LogicalType::VARCHAR),
        "MAP(VARCHAR, VARCHAR)" => LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR),
        other => return Err(crate::unported(format!("settings of type {other}"))),
    })
}

/// `Value::CastAs`.
pub(crate) fn cast_value(value: &Value, target: &LogicalType) -> Result<Value> {
    if value.logical_type() == target {
        return Ok(value.clone());
    }
    let mut result = Vector::new(target.clone(), 1);
    cast_vector(&Vector::constant(value), &mut result, 1, CastMode::Strict)?;
    Ok(result.get_value(0))
}

/// `EXTENSION_SETTINGS` of `extension_entries.hpp`: the settings of the extensions.
const EXTENSION_SETTING_NAMES: &[&str] = &[
    "__iceberg_unsafe_struct_null_default_interp",
    "allow_asterisks_in_http_paths",
    "auto_fallback_to_full_download",
    "aws_default_credentials",
    "aws_network_calls_via_duckdb",
    "azure_account_name",
    "azure_context_caching",
    "azure_credential_chain",
    "azure_endpoint",
    "azure_http_logging",
    "azure_http_logging_redact_headers",
    "azure_http_logging_redact_query_params",
    "azure_http_proxy",
    "azure_http_stats",
    "azure_proxy_password",
    "azure_proxy_user_name",
    "azure_read_buffer_size",
    "azure_read_transfer_chunk_size",
    "azure_read_transfer_concurrency",
    "azure_storage_connection_string",
    "azure_transport_option_type",
    "azure_write_block_size",
    "azure_write_staged_blocks_per_commit",
    "binary_as_string",
    "ca_cert_file",
    "calendar",
    "disable_parquet_prefetching",
    "ducklake_default_data_inlining_row_limit",
    "ducklake_default_version",
    "ducklake_max_retry_count",
    "ducklake_retry_backoff",
    "ducklake_retry_wait_ms",
    "ducklake_target_file_size",
    "ducklake_write_deletion_vectors",
    "enable_curl_server_cert_verification",
    "enable_geoparquet_conversion",
    "enable_global_s3_configuration",
    "enable_legacy_parser",
    "enable_server_cert_verification",
    "extra_http_headers",
    "force_download",
    "force_download_threshold",
    "gcs_user_project",
    "geometry_always_xy",
    "glue_create_bucketed_tables",
    "glue_get_partitions_segments",
    "glue_metadata_cache_scope",
    "glue_network_calls_via_duckdb",
    "gpu_execution",
    "hf_max_per_page",
    "hive_partition_listing_threshold",
    "hnsw_ef_search",
    "hnsw_enable_experimental_persistence",
    "hnsw_prefilter",
    "http_keep_alive",
    "http_retries",
    "http_retry_backoff",
    "http_retry_wait_ms",
    "http_timeout",
    "httpfs_client_implementation",
    "httpfs_connection_caching",
    "httpfs_enable_credential_refresh",
    "iceberg_default_format_version",
    "iceberg_equality_delete_fast_filter",
    "iceberg_logging_post_body_truncate_limit",
    "iceberg_metadata_log_clock_skew_ms",
    "iceberg_test_force_token_expiry",
    "iceberg_unsafe_skip_puffin_verification",
    "iceberg_use_metadata_log",
    "iceberg_use_server_side_scan_planning",
    "json_geometry_format",
    "merge_http_secret_into_s3_request",
    "mysql_bit1_as_boolean",
    "mysql_debug_show_queries",
    "mysql_enable_filter_pushdown",
    "mysql_enable_remote_pushdown",
    "mysql_enable_transactions",
    "mysql_incomplete_dates_as_nulls",
    "mysql_pool_acquire_mode",
    "mysql_pool_connection_idle_timeout_millis",
    "mysql_pool_connection_max_lifetime_millis",
    "mysql_pool_enable_reaper_thread",
    "mysql_pool_enable_thread_local_cache",
    "mysql_pool_health_check_query",
    "mysql_pool_size",
    "mysql_pool_wait_timeout_millis",
    "mysql_session_time_zone",
    "mysql_time_as_time",
    "mysql_tinyint1_as_boolean",
    "parquet_can_have_nan",
    "parquet_metadata_cache",
    "parquet_prefetch_column_gap",
    "parquet_use_page_index",
    "pg_array_as_varchar",
    "pg_connection_cache",
    "pg_connection_limit",
    "pg_debug_show_queries",
    "pg_enable_filter_pushdown",
    "pg_idle_in_transaction_timeout_millis",
    "pg_null_byte_replacement",
    "pg_numeric_as_varchar",
    "pg_numeric_nan_as_null",
    "pg_oauth_token",
    "pg_pages_per_task",
    "pg_pool_acquire_mode",
    "pg_pool_enable_reaper_thread",
    "pg_pool_enable_thread_local_cache",
    "pg_pool_health_check_query",
    "pg_pool_idle_timeout_millis",
    "pg_pool_max_connections",
    "pg_pool_max_lifetime_millis",
    "pg_pool_wait_timeout_millis",
    "pg_query_mode",
    "pg_staleness_query",
    "pg_staleness_query_enabled",
    "pg_statement_timeout_millis",
    "pg_use_binary_copy",
    "pg_use_ctid_scan",
    "pg_use_information_schema_introspection",
    "pg_use_text_protocol",
    "prefetch_all_parquet_files",
    "quack_authentication_function",
    "quack_authorization_function",
    "quack_cache_max_rows",
    "quack_debug_drop_batch",
    "quack_debug_duplicate_sends",
    "quack_debug_emit_delay_ms",
    "quack_debug_fetch_delay_ms",
    "quack_debug_send_delay_ms",
    "quack_default_client_id",
    "quack_default_heartbeat_timeout",
    "quack_enable_reconnects",
    "quack_fetch_read_ahead",
    "quack_loaded_at_us",
    "quack_prepare_inline_rows",
    "quack_rebalance_buffer_bytes",
    "quack_result_ttl",
    "quack_server_keep_alive_timeout",
    "quack_server_max_connections",
    "quack_server_stream_buffer_bytes",
    "quack_target_batch_bytes",
    "rtree_index_scan_min_rows",
    "rtree_index_scan_ratio",
    "s3_access_key_id",
    "s3_allow_recursive_globbing",
    "s3_enable_deprecated_url_parameters",
    "s3_endpoint",
    "s3_kms_key_id",
    "s3_list_concurrency",
    "s3_region",
    "s3_requester_pays",
    "s3_secret_access_key",
    "s3_session_token",
    "s3_uploader_max_filesize",
    "s3_uploader_max_parts_per_file",
    "s3_url_compatibility_mode",
    "s3_url_scheme_aliases",
    "s3_url_style",
    "s3_use_global_endpoint",
    "s3_use_ssl",
    "s3_version_id_pinning",
    "sqlite_all_varchar",
    "sqlite_debug_show_queries",
    "sqlite_disable_multithreaded_scans",
    "timezone",
    "ui_local_port",
    "ui_polling_interval",
    "ui_remote_url",
    "unsafe_disable_etag_checks",
    "unsafe_enable_version_guessing",
    "whoami_hostname",
    "whoami_meta",
    "whoami_name",
    "whoami_provider",
    "whoami_region",
    "whoami_started_at",
];

/// `CompressionType`'s names.
const COMPRESSION_TYPES: [&str; 16] = [
    "AUTO",
    "UNCOMPRESSED",
    "CONSTANT",
    "RLE",
    "DICTIONARY",
    "PFOR",
    "BITPACKING",
    "FSST",
    "CHIMP",
    "PATAS",
    "ALP",
    "ALPRD",
    "ZSTD",
    "ROARING",
    "EMPTY",
    "DICT_FSST",
];

/// `EnumUtil::FromString<CompressionType>` of a name that is none.
fn compression_type_error(input: &str) -> Error {
    let scores = COMPRESSION_TYPES
        .iter()
        .map(|v| (v.to_string(), similarity_rating(v, input)))
        .collect();
    Error::new(
        ErrorKind::NotImplemented,
        format!(
            "Enum value: unrecognized value \"{input}\" for enum \"CompressionType\"\n{}",
            candidates_message(&top_n_strings(scores), "Candidates")
        ),
    )
}

/// The settings whose `OnSet` or `SetGlobal` refuses a NULL.
const NULL_REJECTING: &[&str] = &[
    "allocator_background_threads",
    "allow_community_extensions",
    "allow_extension_repositories",
    "allow_parser_override_extension",
    "allow_persistent_secrets",
    "allow_unredacted_secrets",
    "allow_unsigned_extensions",
    "allowed_configs",
    "allowed_directories",
    "allowed_paths",
    "arrow_output_version",
    "checkpoint_on_detach",
    "debug_checkpoint_abort",
    "debug_force_bitpacking_mode",
    "debug_physical_table_scan_execution_strategy",
    "debug_verify_progress",
    "debug_verify_statement",
    "debug_verify_vector",
    "debug_window_mode",
    "default_block_size",
    "default_transaction_invalidation_policy",
    "dialect_compatibility_mode",
    "enable_block_allocator",
    "enable_external_access",
    "enable_external_file_cache",
    "enable_logging",
    "enable_progress_bar",
    "enable_progress_bar_print",
    "explain_output",
    "extension_directories",
    "external_file_cache_local_max_block_size",
    "external_file_cache_remote_max_block_size",
    "external_file_cache_remote_min_block_size",
    "external_file_cache_request_sizing",
    "fsync_mode",
    "index_scan_percentage",
    "initial_column_segment_size",
    "lambda_syntax",
    "max_temp_directory_size",
    "pin_threads",
    "preserve_identifier_case",
    "progress_bar_time",
    "read_ahead_depth",
    "regex_match_operator_semantics",
    "show_behavior",
    "storage_block_prefetch",
    "table_function_identifier_conversion",
    "tracked_metrics",
    "validate_external_file_cache",
    "warnings_as_errors",
];

/// `OptimizerTypeFromString`'s names.
const OPTIMIZER_TYPES: &[&str] = &[
    "expression_rewriter",
    "filter_pullup",
    "filter_pushdown",
    "empty_result_pullup",
    "cte_filter_pusher",
    "regex_range",
    "in_clause",
    "join_order",
    "deliminator",
    "unnest_rewriter",
    "unused_columns",
    "statistics_propagation",
    "common_subexpressions",
    "common_aggregate",
    "column_lifetime",
    "limit_pushdown",
    "row_group_pruner",
    "top_n",
    "top_n_window_elimination",
    "build_side_probe_side",
    "compressed_materialization",
    "duplicate_groups",
    "reorder_filter",
    "sampling_pushdown",
    "join_filter_pushdown",
    "extension",
    "materialized_cte",
    "aggregate_function_rewriter",
    "late_materialization",
    "cte_inlining",
    "common_subplan",
    "join_elimination",
    "window_self_join",
    "window_join_pushdown",
    "projection_pullup",
    "outer_join_simplification",
    "window_rewriter",
    "partitioned_execution",
    "partial_aggregate_pushdown",
    "remote_pushdown",
    "grouping_sets",
    "type_pushdown",
    "scalar_fn_pushdown",
    "distinct_aggregate_rewrite",
    "aggregate_reuse",
    "projection_placement",
    "redundant_order_keys",
    "disjunctive_join_rewriter",
    "redundant_order",
    "self_join_aggregate",
    "scalar_aggregate_merge",
];

/// The custom `OnSet` callbacks of the settings duckrs honors: they normalize the value.
fn on_set(setting: &SettingDefinition, value: Value) -> Result<Value> {
    let text = || value.try_to_string().map(|s| s.to_lowercase());
    if let Some(enum_name) = setting.enum_name {
        let input = value.try_to_string()?;
        if !setting.enum_values.iter().any(|v| v.eq_ignore_ascii_case(&input)) {
            let scores = setting
                .enum_values
                .iter()
                .map(|v| (v.to_string(), similarity_rating(v, &input)))
                .collect();
            return Err(Error::new(
                ErrorKind::NotImplemented,
                format!(
                    "Enum value: unrecognized value \"{input}\" for enum \"{enum_name}\"\n{}",
                    candidates_message(&top_n_strings(scores), "Candidates")
                ),
            ));
        }
    }
    let invalid = |message: String| Err(Error::new(ErrorKind::InvalidInput, message));
    let number = || value.try_to_string().ok().and_then(|s| s.parse::<f64>().ok());
    match setting.name {
        "threads" if number().is_some_and(|n| n < 1.0) => {
            return Err(Error::new(ErrorKind::Syntax, "Must have at least 1 thread!"));
        }
        "progress_bar_time" if number().is_some_and(|n| n < 0.0) => {
            return invalid("progress_bar_time setting cannot be negative".into());
        }
        "index_scan_percentage" if !number().is_some_and(|n| (0.0..=1.0).contains(&n)) => {
            return invalid("the index scan percentage must be within [0, 1]".into());
        }
        "initial_column_segment_size" => match number() {
            Some(n) if n < 8.0 => {
                return invalid(
                    "The initial column segment size must be at least 8 bytes (default block header storage size)"
                        .into(),
                );
            }
            Some(n) if !(n as u64).is_power_of_two() => {
                return invalid("The initial column segment size must be a power of two".into());
            }
            _ => {}
        },
        "read_ahead_depth" if number().is_some_and(|n| n < -1.0) => {
            return invalid("read_ahead_depth must be -1 (automatic), 0 (disabled) or a positive number".into());
        }
        "http_client_pool_capacity" if number().is_some_and(|n| n <= 0.0) => {
            return invalid(
                "http_client_pool_capacity must be a positive integer, RESET it to return to the automatic value"
                    .into(),
            );
        }
        "profiling_mode" => {
            let mode = text()?;
            if !matches!(mode.as_str(), "standard" | "detailed" | "all") {
                return Err(Error::new(
                    ErrorKind::Parser,
                    format!("Unrecognized profiling mode \"{mode}\", supported formats: [standard, detailed, all]"),
                ));
            }
        }
        // `DisabledCompressionMethodsSetting`: what every column can fall back to stays
        "disabled_compression_methods" => {
            for entry in value.try_to_string()?.split(',') {
                let entry = entry.trim().to_lowercase();
                if entry == "none" {
                    break;
                }
                if entry.is_empty() {
                    continue;
                }
                if !COMPRESSION_TYPES.iter().any(|t| t.eq_ignore_ascii_case(&entry)) {
                    return Err(compression_type_error(&entry));
                }
                if matches!(entry.as_str(), "auto" | "constant" | "empty" | "uncompressed") {
                    return invalid(format!("Compression method {entry} cannot be disabled"));
                }
            }
        }
        // `DBConfig::AddAllowedConfig`
        "allowed_configs" => {
            if let duckrs_common::value::ValueData::List(names) = value.data() {
                for name in names {
                    let name = name.try_to_string()?;
                    if name.is_empty() {
                        return invalid("Cannot provide an empty string for allowed_configs".into());
                    }
                    if name.eq_ignore_ascii_case("allowed_configs") || name.eq_ignore_ascii_case("lock_configuration") {
                        return invalid(format!("Cannot include \"{name}\" in allowed_configs"));
                    }
                    if find_setting(&name).is_none()
                        && !EXTENSION_SETTING_NAMES.iter().any(|s| s.eq_ignore_ascii_case(&name))
                    {
                        return invalid(format!("Unknown configuration option \"{name}\" in allowed_configs"));
                    }
                }
            }
        }
        // `DisabledOptimizersSetting::SetGlobal`: `OptimizerTypeFromString` of each entry
        "disabled_optimizers" => {
            let list = value.try_to_string()?;
            for entry in list.split(',') {
                let entry = entry.trim().to_lowercase();
                if !entry.is_empty() && !OPTIMIZER_TYPES.contains(&entry.as_str()) {
                    let names: Vec<String> = OPTIMIZER_TYPES.iter().map(|s| s.to_string()).collect();
                    return Err(Error::new(
                        ErrorKind::Parser,
                        format!(
                            "Optimizer type \"{entry}\" not recognized\n{}",
                            candidates_error_message(&names, &entry, "Candidate optimizers")
                        ),
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(match setting.name {
        // `ForceCompressionSetting::OnSet`: a `CompressionType` that can be forced
        "force_compression" => {
            let compression = text()?;
            if compression == "none" || compression == "auto" {
                return Ok(Value::varchar("auto"));
            }
            let input = value.try_to_string()?;
            let Some(found) = COMPRESSION_TYPES.iter().find(|t| t.eq_ignore_ascii_case(&compression)) else {
                return Err(compression_type_error(&input));
            };
            match *found {
                "CONSTANT" | "EMPTY" => {
                    return Err(Error::new(
                        ErrorKind::Parser,
                        "auto / constant cannot be used for force_compression",
                    ));
                }
                "CHIMP" | "PATAS" => {
                    let name = if *found == "CHIMP" { "Chimp" } else { "Patas" };
                    return Err(Error::new(
                        ErrorKind::Parser,
                        format!("Attempted to force a deprecated compression type ({name})"),
                    ));
                }
                _ => value,
            }
        }
        // `DefaultCollationSetting::OnSet`: the collation has to exist
        "default_collation" => {
            crate::planner::collation::test_collation(&value.try_to_string()?)?;
            value
        }
        "default_null_order" => {
            let parameter = text()?;
            Value::varchar(match parameter.as_str() {
                "nulls_first" | "nulls first" | "null first" | "first" => "NULLS_FIRST",
                "nulls_last" | "nulls last" | "null last" | "last" => "NULLS_LAST",
                "nulls_first_on_asc_last_on_desc" | "sqlite" | "mysql" => "NULLS_FIRST_ON_ASC_LAST_ON_DESC",
                "nulls_last_on_asc_first_on_desc" | "postgres" => "NULLS_LAST_ON_ASC_FIRST_ON_DESC",
                _ => {
                    return Err(Error::new(
                        ErrorKind::Parser,
                        format!(
                            "Unrecognized parameter for option NULL_ORDER \"{parameter}\", expected either NULLS \
                             FIRST, NULLS LAST, SQLite, MySQL or Postgres"
                        ),
                    ));
                }
            })
        }
        "default_order" => {
            let parameter = text()?;
            Value::varchar(match parameter.as_str() {
                "ascending" | "asc" => "ASC",
                "descending" | "desc" => "DESC",
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Unrecognized parameter for option DEFAULT_ORDER \"{parameter}\". Expected ASC or DESC."
                        ),
                    ));
                }
            })
        }
        // `MaxMemorySetting::SetGlobal` and `GetSetting`: a size, shown as one
        "max_memory" => {
            use crate::execution::temporary::{format_bytes, parse_memory_limit, physical_memory};
            let input = value.try_to_string()?;
            let bytes = match input.strip_suffix('%') {
                Some(percentage) => {
                    let percentage: f64 = percentage
                        .trim()
                        .parse()
                        .ok()
                        .filter(|p| (0.0..=100.0).contains(p))
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::InvalidInput,
                                format!("Unable to parse valid percentage (input: {input})"),
                            )
                        })?;
                    let memory = physical_memory().unwrap_or(u64::MAX) as f64;
                    // C++ rounds a tiny percentage down to whole bytes, but never to none
                    ((percentage * memory / 100.0) as u64).max(1)
                }
                None => parse_memory_limit(&input)?.unwrap_or(u64::MAX),
            };
            Value::varchar(format_bytes(bytes))
        }
        // `MaxTempDirectorySizeSetting::SetGlobal`
        "max_temp_directory_size" => {
            use crate::execution::temporary::{format_bytes, parse_memory_limit};
            if value.is_null() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "max_temp_directory_size setting cannot be NULL",
                ));
            }
            let input = value.try_to_string()?;
            if input == crate::execution::temporary::DEFAULT_TEMP_DIRECTORY_SIZE {
                value
            } else {
                Value::varchar(format_bytes(parse_memory_limit(&input)?.unwrap_or(u64::MAX)))
            }
        }
        _ => value,
    })
}

/// The default value of a setting, if it has one (`DBConfig::TryGetDefaultValue`).
fn default_value(setting: &SettingDefinition) -> Option<Value> {
    let text = setting.default_value?;
    let ty = input_type(setting).ok()?;
    let value = cast_value(&Value::varchar(text), &ty).ok()?;
    on_set(setting, value.clone()).ok().or(Some(value))
}

/// Setting values of one scope.
#[derive(Debug, Default, Clone)]
pub struct SettingValues(HashMap<usize, Value>);

/// `PhysicalSet::GetSettingScope`: the scope `SET` without a scope uses, and the checks of an
/// explicit one. `true` means global.
fn resolve_scope(setting: &SettingDefinition, scope: SetScope) -> Result<bool> {
    let invalid = |message: String| Err(Error::new(ErrorKind::InvalidInput, message));
    match scope {
        SetScope::Automatic => Ok(matches!(
            setting.scope,
            SettingScope::GlobalOnly | SettingScope::GlobalDefault
        )),
        SetScope::Local | SetScope::Session if setting.scope == SettingScope::GlobalOnly => invalid(format!(
            "Setting \"{}\" cannot be set as a session variable - it can only be set globally",
            setting.name
        )),
        SetScope::Local | SetScope::Session => Ok(false),
        SetScope::Global if setting.scope == SettingScope::LocalOnly => invalid(format!(
            "Setting \"{}\" cannot be set as a global variable - it can only be set per session",
            setting.name
        )),
        SetScope::Global => Ok(true),
        SetScope::Variable => Err(crate::unported("SET VARIABLE")),
    }
}

/// `PhysicalSet::SetVariable`: validates `value` for the setting `name` and stores it in the
/// global or session values.
pub fn set_variable(
    name: &str,
    scope: SetScope,
    value: &Value,
    global: &mut SettingValues,
    session: &mut SettingValues,
) -> Result<()> {
    if scope == SetScope::Variable {
        return Err(crate::unported("SET VARIABLE"));
    }
    let (index, setting) = find_setting(name).ok_or_else(|| unrecognized(name))?;
    let is_global = resolve_scope(setting, scope)?;
    if value.is_null() {
        let invalid = |message: String| Err(Error::new(ErrorKind::InvalidInput, message));
        match setting.name {
            "threads" | "external_threads" | "async_threads" | "ordered_aggregate_threshold" => {
                return invalid(format!("{} must be a positive integer", setting.name));
            }
            "perfect_ht_threshold" => return invalid("perfect_ht_threshold must be an integer".into()),
            "http_client_pool_capacity" => {
                return invalid("http_client_pool_capacity must be a positive integer".into());
            }
            name if NULL_REJECTING.contains(&name) => return invalid(format!("{name} setting cannot be NULL")),
            _ => {}
        }
    }
    // `Value::CastAs`: a value that does not cast is not one of the setting's
    let value = cast_value(value, &input_type(setting)?).map_err(|error| match error.kind {
        ErrorKind::Conversion => Error::new(
            ErrorKind::InvalidInput,
            format!("Failed to cast value: {}", error.message),
        ),
        _ => error,
    })?;
    let value = on_set(setting, value)?;
    let value = file_access_on_set(setting.name, value, global, session)?;
    if UNHONORED.contains(&setting.name) && Some(&value) != default_value(setting).as_ref() {
        return Err(crate::unported(format!("the {} setting", setting.name)));
    }
    let target = if is_global { global } else { session };
    target.0.insert(index, value);
    Ok(())
}

/// `PhysicalReset`: forgets the value of `name` in the scope `RESET` names.
pub fn reset_variable(
    name: &str,
    scope: SetScope,
    global: &mut SettingValues,
    session: &mut SettingValues,
) -> Result<()> {
    if scope == SetScope::Variable {
        return Err(crate::unported("RESET VARIABLE"));
    }
    let (index, setting) = find_setting(name).ok_or_else(|| unrecognized(name))?;
    if matches!(setting.name, "allowed_directories" | "allowed_paths") {
        check_external_access_enabled(setting.name, global, session)?;
    }
    if setting.name == "temp_directory" {
        check_temp_directory_change(global, session)?;
    }
    if resolve_scope(setting, scope)? {
        global.0.remove(&index);
    } else {
        session.0.remove(&index);
    }
    Ok(())
}

/// Whether `enable_external_access` is on.
pub fn external_access_enabled(global: &SettingValues, session: &SettingValues) -> bool {
    current_value("enable_external_access", global, session)
        .is_none_or(|value| !matches!(value.data(), duckrs_common::value::ValueData::Bool(false)))
}

/// What may still be read and written while `enable_external_access` is off; `None` while it is on.
pub fn file_access(global: &SettingValues, session: &SettingValues) -> Option<FileAccess> {
    if external_access_enabled(global, session) {
        return None;
    }
    let list = |name: &str| -> std::collections::BTreeSet<String> {
        current_value(name, global, session)
            .map(|value| list_items(&value))
            .unwrap_or_default()
            .iter()
            .filter_map(|value| value.try_to_string().ok())
            .collect()
    };
    Some(FileAccess {
        paths: list("allowed_paths"),
        directories: list("allowed_directories"),
    })
}

/// Adds `path` to `allowed_paths` (`DBConfig::AddAllowedPath`).
pub fn add_allowed_path(path: &str, global: &mut SettingValues) {
    let Some((index, _)) = find_setting("allowed_paths") else {
        return;
    };
    let mut paths: std::collections::BTreeSet<String> = global
        .0
        .get(&index)
        .map(list_items)
        .unwrap_or_default()
        .iter()
        .filter_map(|value| value.try_to_string().ok())
        .collect();
    paths.insert(sanitize_allowed_path(path));
    global.0.insert(index, varchar_list(paths));
}

/// The items of a LIST value; none for anything else.
fn list_items(value: &Value) -> Vec<Value> {
    match value.data() {
        duckrs_common::value::ValueData::List(items) => items.clone(),
        _ => Vec::new(),
    }
}

fn varchar_list(items: impl IntoIterator<Item = String>) -> Value {
    Value::list(LogicalType::VARCHAR, items.into_iter().map(Value::varchar).collect()).expect("a list of VARCHAR")
}

fn check_external_access_enabled(name: &str, global: &SettingValues, session: &SettingValues) -> Result<()> {
    if external_access_enabled(global, session) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::InvalidInput,
        format!("Cannot change {name} when enable_external_access is disabled"),
    ))
}

/// `TempDirectorySetting::SetGlobal` and `ResetGlobal`.
fn check_temp_directory_change(global: &SettingValues, session: &SettingValues) -> Result<()> {
    if external_access_enabled(global, session) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::Permission,
        "Modifying the temp_directory has been disabled by configuration",
    ))
}

/// `EnableExternalAccessSetting::OnSet`, `AllowedDirectoriesSetting::SetGlobal` and
/// `AllowedPathsSetting::SetGlobal`: the paths are kept as `SanitizeAllowedPath` makes them.
fn file_access_on_set(name: &str, value: Value, global: &SettingValues, session: &SettingValues) -> Result<Value> {
    match name {
        "enable_external_access" => {
            if matches!(value.data(), duckrs_common::value::ValueData::Bool(true)) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Cannot enable external access while database is running",
                ));
            }
            Ok(value)
        }
        "temp_directory" => {
            check_temp_directory_change(global, session)?;
            Ok(value)
        }
        "allowed_directories" | "allowed_paths" => {
            check_external_access_enabled(name, global, session)?;
            let mut items = std::collections::BTreeSet::new();
            for item in list_items(&value) {
                let mut path = sanitize_allowed_path(&item.try_to_string()?);
                if name == "allowed_directories" {
                    if path.is_empty() {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "Cannot provide an empty string for allowed_directory",
                        ));
                    }
                    if !path.ends_with('/') {
                        path.push('/');
                    }
                }
                items.insert(path);
            }
            Ok(varchar_list(items))
        }
        _ => Ok(value),
    }
}

/// Stores `value` as the global value of `name`, as it is: for the settings whose value lives
/// elsewhere and is read from there (`GetSetting`), such as the logging settings.
pub fn store_global(name: &str, value: Value, global: &mut SettingValues) {
    if let Some((index, _)) = find_setting(name) {
        global.0.insert(index, value);
    }
}

/// The current value of a setting: the session's, else the database's, else the default.
/// `current_setting`.
pub fn get_setting(name: &str, global: &SettingValues, session: &SettingValues) -> Result<Value> {
    if find_setting(name).is_none() {
        return Err(unrecognized(name));
    }
    Ok(current_value(name, global, session).unwrap_or_else(Value::sql_null))
}

pub fn current_value(name: &str, global: &SettingValues, session: &SettingValues) -> Option<Value> {
    let (index, setting) = find_setting(name)?;
    session
        .0
        .get(&index)
        .or_else(|| global.0.get(&index))
        .cloned()
        .or_else(|| default_value(setting))
}

/// A row of `duckdb_settings()`.
#[derive(Debug, Clone)]
pub struct SettingRow {
    pub name: &'static str,
    pub value: Option<Value>,
    pub description: &'static str,
    pub input_type: &'static str,
    /// `GLOBAL` or `LOCAL`
    pub scope: &'static str,
    pub aliases: &'static [&'static str],
    pub is_debug: bool,
    pub is_deprecated: bool,
}

/// `DuckDBSettingsInit`: every setting with its current value, each alias as a row of its
/// own, sorted by name.
pub fn list_settings(global: &SettingValues, session: &SettingValues) -> Vec<SettingRow> {
    let mut rows = Vec::new();
    for (index, setting) in SETTINGS.iter().chain(&EXTENSION_SETTINGS).enumerate() {
        let extension = index >= SETTINGS.len();
        let mut scope = if extension || setting.scope != SettingScope::LocalOnly {
            "GLOBAL"
        } else {
            "LOCAL"
        };
        let value = if let Some(value) = session.0.get(&index) {
            scope = "LOCAL";
            Some(value.clone())
        } else if let Some(value) = global.0.get(&index) {
            scope = "GLOBAL";
            Some(value.clone())
        } else {
            default_value(setting)
        };
        let row = SettingRow {
            name: setting.name,
            value,
            description: setting.description,
            input_type: setting.input_type,
            scope,
            aliases: setting.aliases,
            is_debug: setting.is_debug,
            is_deprecated: setting.is_deprecated,
        };
        for alias in setting.aliases {
            rows.push(SettingRow {
                name: alias,
                aliases: &[],
                ..row.clone()
            });
        }
        rows.push(row);
    }
    rows.sort_by(|a, b| a.name.cmp(b.name));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_settings_rows() {
        let (mut global, mut session) = (SettingValues::default(), SettingValues::default());
        let rows = list_settings(&global, &session);
        assert!(rows.windows(2).all(|w| w[0].name <= w[1].name));
        // an alias is a row of its own, without aliases
        let alias = rows.iter().find(|r| r.name == "null_order").unwrap();
        assert!(alias.aliases.is_empty());
        let row = rows.iter().find(|r| r.name == "default_null_order").unwrap();
        assert_eq!(row.aliases, &["null_order"]);
        assert_eq!(row.value, Some(Value::varchar("NULLS_LAST")));
        assert!(!row.description.is_empty());
        set_variable(
            "default_null_order",
            SetScope::Session,
            &Value::varchar("first"),
            &mut global,
            &mut session,
        )
        .unwrap();
        let rows = list_settings(&global, &session);
        let row = rows.iter().find(|r| r.name == "default_null_order").unwrap();
        assert_eq!(
            (row.scope, row.value.clone()),
            ("LOCAL", Some(Value::varchar("NULLS_FIRST")))
        );
        // VARCHAR[] settings can be stored; allowed directories are kept sanitized, ending in `/`
        set_variable(
            "extension_directories",
            SetScope::Automatic,
            &Value::list(LogicalType::VARCHAR, vec![Value::varchar("/tmp")]).unwrap(),
            &mut global,
            &mut session,
        )
        .unwrap();
        set_variable(
            "allowed_directories",
            SetScope::Automatic,
            &Value::list(LogicalType::VARCHAR, vec![Value::varchar("/tmp/a/../b")]).unwrap(),
            &mut global,
            &mut session,
        )
        .unwrap();
        let access = |g: &SettingValues, s: &SettingValues| file_access(g, s);
        assert_eq!(access(&global, &session), None);
        set_variable(
            "enable_external_access",
            SetScope::Automatic,
            &Value::boolean(false),
            &mut global,
            &mut session,
        )
        .unwrap();
        let policy = access(&global, &session).unwrap();
        assert!(policy.can_access("/tmp/b/x.csv", false));
        assert!(policy.can_access("/tmp/b", true));
        assert!(!policy.can_access("/tmp/b/../c.csv", false));
        assert!(!policy.can_access("/tmp/bc.csv", false));
        let err = set_variable(
            "enable_external_access",
            SetScope::Automatic,
            &Value::boolean(true),
            &mut global,
            &mut session,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Cannot enable external access"), "{err}");
        let err = reset_variable("allowed_directories", SetScope::Automatic, &mut global, &mut session).unwrap_err();
        assert!(err.to_string().contains("enable_external_access is disabled"), "{err}");
    }

    #[test]
    fn set_and_reset() {
        let (mut global, mut session) = (SettingValues::default(), SettingValues::default());
        let current = |g: &SettingValues, s: &SettingValues| current_value("default_null_order", g, s).unwrap();
        assert_eq!(current(&global, &session), Value::varchar("NULLS_LAST"));
        set_variable(
            "null_order",
            SetScope::Automatic,
            &Value::varchar("postgres"),
            &mut global,
            &mut session,
        )
        .unwrap();
        assert_eq!(
            current(&global, &session),
            Value::varchar("NULLS_LAST_ON_ASC_FIRST_ON_DESC")
        );
        set_variable(
            "default_null_order",
            SetScope::Session,
            &Value::varchar("first"),
            &mut global,
            &mut session,
        )
        .unwrap();
        assert_eq!(current(&global, &session), Value::varchar("NULLS_FIRST"));
        reset_variable("default_null_order", SetScope::Session, &mut global, &mut session).unwrap();
        assert_eq!(
            current(&global, &session),
            Value::varchar("NULLS_LAST_ON_ASC_FIRST_ON_DESC")
        );
        // threads is a global-only setting of type BIGINT
        set_variable(
            "threads",
            SetScope::Automatic,
            &Value::integer(4),
            &mut global,
            &mut session,
        )
        .unwrap();
        let err = set_variable(
            "threads",
            SetScope::Local,
            &Value::integer(4),
            &mut global,
            &mut session,
        )
        .unwrap_err();
        assert!(err.to_string().contains("can only be set globally"), "{err}");
        let err = set_variable(
            "thread",
            SetScope::Automatic,
            &Value::integer(4),
            &mut global,
            &mut session,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Catalog Error: unrecognized configuration parameter thread\n\nDid you mean: \"threads\""),
            "{err}"
        );
        // a result-changing setting that is not implemented
        let err = set_variable(
            "integer_division",
            SetScope::Automatic,
            &Value::boolean(true),
            &mut global,
            &mut session,
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not yet ported"), "{err}");
        set_variable(
            "integer_division",
            SetScope::Automatic,
            &Value::boolean(false),
            &mut global,
            &mut session,
        )
        .unwrap();
    }
}
