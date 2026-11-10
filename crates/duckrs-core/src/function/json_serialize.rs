// Ported from duckdb/extension/json/json_functions/json_serialize_sql.cpp @ rust-port-baseline
//
//! `json_serialize_sql`, `json_deserialize_sql` and what `json_execute_serialized_sql` reads: the parsed
//! SELECT statements of SQL as JSON, by the serializer of the file format in its JSON mode, and back.

use super::ScalarCallback;
use super::json::{Json, json_type, parse};
use super::nested::{bound_function, row_wise};
use crate::planner::expression::BoundExpr;
use crate::storage::serialization::{convert, generated as g, to_stored, verify};
use duckrs_common::serializer::{
    BinaryDeserializer, BinarySerializer, Deserialize, SerializationOptions, StorageVersion,
};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast;

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message.into())
}

fn parser_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Parser, message.into())
}

fn is_true(value: &Value) -> bool {
    matches!(value.data(), ValueData::Bool(true))
}

pub fn is_serialize_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "json_serialize_sql" | "json_deserialize_sql"
    )
}

/// `JsonSerializeBind` and the binding of `json_deserialize_sql`. The options of `json_serialize_sql`
/// stay arguments, constants after the SQL, with the storage version to write for at the end.
pub fn bind_function(
    name: &str,
    names: &[String],
    mut children: Vec<BoundExpr>,
    target: StorageVersion,
) -> Result<BoundExpr> {
    let lower = name.to_ascii_lowercase();
    if lower == "json_deserialize_sql" {
        if children.len() != 1 || names.iter().any(|name| !name.is_empty()) {
            return Err(binder_error(format!(
                "No function matches the given name and argument types 'json_deserialize_sql({})'",
                children
                    .iter()
                    .map(|c| c.return_type.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let child = children.pop().expect("one");
        let child = if child.return_type.id() == LogicalTypeId::Varchar {
            child
        } else {
            child.cast_to(&json_type(), false)
        };
        return Ok(bound_function(
            &lower,
            vec![child],
            LogicalType::VARCHAR,
            json_deserialize_sql,
        ));
    }
    if children.is_empty() {
        return Err(binder_error("json_serialize_sql takes at least one argument"));
    }
    if children[0].return_type.id() != LogicalTypeId::Varchar {
        return Err(Error::new(
            ErrorKind::InvalidType,
            "json_serialize_sql first argument must be a VARCHAR",
        ));
    }
    let mut flags = [false; 4];
    const OPTIONS: [&str; 4] = ["skip_null", "skip_empty", "skip_default", "format"];
    for (child, name) in children.iter().zip(names).skip(1) {
        let Some(at) = OPTIONS.iter().position(|option| option == name) else {
            return Err(binder_error(format!("json_serialize_sql: Unknown argument {name}")));
        };
        if child.return_type.id() != LogicalTypeId::Boolean {
            return Err(binder_error(format!(
                "json_serialize_sql: '{name}' argument must be a boolean"
            )));
        }
        let value = crate::execution::expression_executor::evaluate_scalar(child)?;
        flags[at] = is_true(&value);
    }
    let sql = children.swap_remove(0);
    let mut arguments = vec![sql];
    arguments.extend(flags.map(|flag| BoundExpr::constant(Value::boolean(flag))));
    arguments.push(BoundExpr::constant(Value::ubigint(target.0)));
    let callback: ScalarCallback = json_serialize_sql;
    Ok(bound_function(&lower, arguments, json_type(), callback))
}

/// `JsonSerializeFunction`.
fn json_serialize_sql(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(json_type()));
        }
        let flag = |at: usize| is_true(&row[at]);
        let target = StorageVersion(row[5].as_i128().unwrap_or(0) as u64);
        let options = SerializeOptions {
            skip_null: flag(1),
            skip_empty: flag(2),
            skip_default: flag(3),
            format: flag(4),
            target,
        };
        Value::varchar(serialize_sql(&row[0].try_to_string()?, &options)).reinterpret(&json_type())
    })
}

pub struct SerializeOptions {
    pub skip_null: bool,
    pub skip_empty: bool,
    pub skip_default: bool,
    pub format: bool,
    pub target: StorageVersion,
}

/// The document `json_serialize_sql` gives: the statements, or the error that parsing them gave.
pub fn serialize_sql(sql: &str, options: &SerializeOptions) -> String {
    let document = match serialize_statements(sql, options) {
        Ok(statements) => Json::Object(vec![
            ("error".to_string(), Json::Bool(false)),
            ("statements".to_string(), Json::Array(statements)),
        ]),
        Err(error) => {
            let mut members = vec![
                ("error".to_string(), Json::Bool(true)),
                ("error_type".to_string(), Json::Str(error.kind.name().to_lowercase())),
                ("error_message".to_string(), Json::Str(error.message.clone())),
            ];
            if let Some(position) = error.position {
                members.push(("position".to_string(), Json::Str(position.to_string())));
            }
            Json::Object(members)
        }
    };
    let mut out = String::new();
    if options.format {
        document.pretty(0, &mut out);
    } else {
        document.write(&mut out);
    }
    out
}

fn serialize_statements(sql: &str, options: &SerializeOptions) -> Result<Vec<Json>> {
    let statements = duckrs_parser::parse_ast(sql)?;
    let serialization = SerializationOptions {
        serialize_default_values: !options.skip_default,
        target: options.target,
    };
    statements
        .iter()
        .map(|statement| {
            let ast::Statement::Select(select) = statement else {
                return Err(Error::not_implemented(
                    "Only SELECT statements can be serialized to json!",
                ));
            };
            let stored = to_stored::select_statement(select)?;
            Ok(BinarySerializer::serialize_json(
                &stored,
                serialization,
                options.skip_null,
                options.skip_empty,
            ))
        })
        .collect()
}

/// `DeserializeSelectStatement`: the statements of a document of `json_serialize_sql`.
pub fn deserialize_select_statements(text: &str) -> Result<Vec<ast::SelectStatement>> {
    let root = parse(text).map_err(|_| parser_error("Could not parse json"))?;
    let member = |name: &str| match &root {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    };
    if matches!(member("error"), Some(Json::Bool(true))) {
        return match (member("error_type"), member("error_message")) {
            (Some(Json::Str(kind)), Some(Json::Str(message))) => {
                Err(parser_error(format!("Error parsing json: {kind}: {message}")))
            }
            _ => Err(parser_error(
                "Error parsing json, expected error property to contain 'error_type' and 'error_message'",
            )),
        };
    }
    let Some(Json::Array(statements)) = member("statements") else {
        return Err(parser_error("Error parsing json: no statements array"));
    };
    if statements.is_empty() {
        return Err(parser_error("Error parsing json: no statements"));
    }
    statements
        .iter()
        .map(|statement| {
            let mut d = BinaryDeserializer::from_json(statement).disallow_pointers();
            let stored = g::SelectStatement::read_value(&mut d)?;
            if stored.node.is_none() {
                return Err(parser_error("Error parsing json: no select node found in json"));
            }
            verify::select_statement(&stored)?;
            convert::select_statement(Some(Box::new(stored)))
        })
        .collect()
}

/// `JsonDeserializeFunction`: the statements as SQL, separated by semicolons.
fn json_deserialize_sql(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::VARCHAR));
        }
        let statements = deserialize_select_statements(&row[0].try_to_string()?)?;
        let sql: Vec<String> = statements.iter().map(ToString::to_string).collect();
        Ok(Value::varchar(sql.join("; ")))
    })
}

/// `json_execute_serialized_sql`: the one statement of a document, to run.
pub fn execute_serialized_statement(value: &Value, pragma: bool) -> Result<ast::SelectStatement> {
    if value.is_null() {
        return Err(binder_error("json_execute_serialized_sql cannot execute NULL plan"));
    }
    let mut statements = deserialize_select_statements(&value.try_to_string()?)?;
    if statements.len() != 1 {
        return Err(binder_error(if pragma {
            "json_execute_serialized_sql pragma expects exactly one statement"
        } else {
            "json_execute_serialized_sql expects exactly one statement"
        }));
    }
    Ok(statements.pop().expect("one"))
}
