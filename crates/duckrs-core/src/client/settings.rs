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
}

/// Settings that change query results or how SQL is read, whose effect duckrs does not
/// implement yet.
const UNHONORED: &[&str] = &[
    "access_mode",
    "active_grammar_extensions",
    "allow_parser_override_extension",
    "current_dialect",
    "default_collation",
    "dialect_compatibility_mode",
    "disable_timestamptz_casts",
    "error_on_division_by_zero",
    "errors_as_json",
    "file_search_path",
    "home_directory",
    "ieee_floating_point_ops",
    "integer_division",
    "legacy_disable_null_type",
    "lock_configuration",
    "old_implicit_casting",
    "order_by_non_integer_literal",
    "preserve_identifier_case",
    "regex_match_operator_semantics",
    "scalar_subquery_error_on_multiple_rows",
    "schema",
    "search_path",
    "show_behavior",
    "table_function_identifier_conversion",
];

/// `DBConfig::GetOptionByName`: a setting by its name or an alias, case-insensitively.
pub fn find_setting(name: &str) -> Option<(usize, &'static SettingDefinition)> {
    SETTINGS
        .iter()
        .enumerate()
        .find(|(_, s)| s.name.eq_ignore_ascii_case(name) || s.aliases.iter().any(|a| a.eq_ignore_ascii_case(name)))
}

/// `Catalog::UnrecognizedConfigurationError`.
fn unrecognized(name: &str) -> Error {
    let mut names: Vec<String> = SETTINGS.iter().map(|s| s.name.to_string()).collect();
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
        "MAP(VARCHAR, VARCHAR)" => LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR),
        other => return Err(crate::unported(format!("settings of type {other}"))),
    })
}

/// `Value::CastAs`.
fn cast_value(value: &Value, target: &LogicalType) -> Result<Value> {
    if value.logical_type() == target {
        return Ok(value.clone());
    }
    let mut result = Vector::new(target.clone(), 1);
    cast_vector(&Vector::constant(value), &mut result, 1, CastMode::Strict)?;
    Ok(result.get_value(0))
}

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
    Ok(match setting.name {
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
    let value = on_set(setting, cast_value(value, &input_type(setting)?)?)?;
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
    if resolve_scope(setting, scope)? {
        global.0.remove(&index);
    } else {
        session.0.remove(&index);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
