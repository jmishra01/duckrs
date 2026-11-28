// Ported from duckdb/src/function/table/system/external_resource_types.cpp,
// duckdb/src/function/table/system/external_resources.cpp and
// duckdb/src/function/table/system/create_external_resource.cpp @ rust-port-baseline
//
//! The external resource table functions. What they read and write (the type registry and the resource
//! manager of the instance) and the callbacks they run on an internal connection are the client's; the
//! binder checks the arguments, as each function's bind does, and binds the rows the client gives.

use super::{Binder, binder_error};
use crate::planner::logical::LogicalOperator;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{FunctionExpr, TableRef};

/// What a call of an external resource table function gives: its rows, or the query it stands for
/// (`bind_replace`, as `duckdb_external_resources(discover := true)` has).
pub enum ExternalResourceRows {
    Rows(Vec<Vec<Value>>),
    Query(String),
}

/// The client's side of the external resource table functions: the instance's `ExternalResourceTypeRegistry` and
/// `ExternalResourcesManager`, and the connections the callbacks run on.
pub trait ExternalResourceTables: Send + Sync {
    /// The call of `function` with its arguments, cast to the types of its signature: the positional ones in
    /// order, the named ones by their names in lower case.
    fn call(&self, function: &str, positional: Vec<Value>, named: Vec<(String, Value)>)
    -> Result<ExternalResourceRows>;
}

/// The external resource table functions.
pub const EXTERNAL_RESOURCE_FUNCTIONS: &[&str] = &[
    "register_external_resource_type",
    "duckdb_external_resource_types",
    "create_external_resource",
    "destroy_external_resource",
    "register_external_resource",
    "deregister_external_resource",
    "duckdb_external_resources",
];

fn handle_type() -> LogicalType {
    LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR)
}

/// The type of a parameter; `None` takes any type (`LogicalType::ANY`).
type ParameterType = Option<LogicalType>;

/// The positional parameters and the named ones of `function`.
fn signature(function: &str) -> (Vec<ParameterType>, Vec<(&'static str, ParameterType)>) {
    let varchar = || Some(LogicalType::VARCHAR);
    match function {
        "register_external_resource_type" => (
            vec![varchar()],
            vec![
                ("kind", varchar()),
                ("create_function", varchar()),
                ("status_function", varchar()),
                ("destroy_function", varchar()),
                ("resolve_function", varchar()),
                ("list_function", varchar()),
            ],
        ),
        "create_external_resource" => (
            vec![varchar()],
            vec![
                ("params", Some(handle_type())),
                ("resource_name", varchar()),
                ("handle", None),
                ("teardown_on_failure", Some(LogicalType::BOOLEAN)),
                ("timeout_seconds", Some(LogicalType::BIGINT)),
                ("poll_interval_seconds", Some(LogicalType::BIGINT)),
            ],
        ),
        "destroy_external_resource" => (vec![varchar(), None], Vec::new()),
        "register_external_resource" => (
            vec![varchar(), varchar(), Some(handle_type())],
            vec![
                ("uri", varchar()),
                ("attached_db_type", varchar()),
                ("deleter_function", varchar()),
            ],
        ),
        "deregister_external_resource" => (vec![varchar()], Vec::new()),
        "duckdb_external_resources" => (
            Vec::new(),
            vec![("discover", Some(LogicalType::BOOLEAN)), ("type", varchar())],
        ),
        _ => (Vec::new(), Vec::new()),
    }
}

/// The columns of the result of `function`.
pub fn external_resource_columns(function: &str) -> Vec<(&'static str, LogicalType)> {
    let varchar = LogicalType::VARCHAR;
    match function {
        "duckdb_external_resource_types" => [
            "name",
            "kind",
            "create_function",
            "status_function",
            "destroy_function",
            "resolve_function",
            "list_function",
            "origin",
        ]
        .into_iter()
        .map(|name| (name, varchar.clone()))
        .collect(),
        "create_external_resource" => vec![
            ("handle", handle_type()),
            ("uri", varchar.clone()),
            ("attached_db_type", varchar.clone()),
            ("result", handle_type()),
            ("deleter_function", varchar),
            ("deleter_payload", handle_type()),
        ],
        "duckdb_external_resources" => vec![
            ("name", varchar.clone()),
            ("type", varchar.clone()),
            ("managed", LogicalType::BOOLEAN),
            ("state", varchar.clone()),
            ("reference", varchar.clone()),
            ("uri", varchar.clone()),
            ("attached_db_type", varchar),
            ("handle", handle_type()),
        ],
        _ => vec![("Success", LogicalType::BOOLEAN)],
    }
}

impl Binder<'_> {
    /// One of [`EXTERNAL_RESOURCE_FUNCTIONS`]: the arguments checked and cast as its signature says, and the rows
    /// (or the query) the client gives for them.
    pub(super) fn bind_external_resource_function(
        &mut self,
        name: &str,
        call: &FunctionExpr,
        table_ref: &TableRef,
    ) -> Result<LogicalOperator> {
        let (positional_types, named_types) = signature(name);
        let mut positional = Vec::new();
        let mut named = Vec::new();
        for argument in call.args.iter().map(super::table_functions::named_argument) {
            if argument.name.is_empty() {
                let Some(ty) = positional_types.get(positional.len()) else {
                    return Err(binder_error(format!(
                        "Function \"{name}\" expects {} positional argument{}, got {}",
                        positional_types.len(),
                        if positional_types.len() == 1 { "" } else { "s" },
                        positional.len() + 1
                    )));
                };
                let value = self.constant_argument(&argument.expr)?;
                positional.push(match ty {
                    Some(ty) if !value.is_null() => crate::function::cast::cast_value(&value, ty)?,
                    _ => value,
                });
                continue;
            }
            let parameter = argument.name.to_lowercase();
            let Some((_, ty)) = named_types.iter().find(|(known, _)| *known == parameter) else {
                return Err(binder_error(format!(
                    "Invalid named parameter \"{}\" for function {name}",
                    argument.name
                )));
            };
            let value = self.constant_argument(&argument.expr)?;
            let value = match ty {
                Some(ty) if !value.is_null() => crate::function::cast::cast_value(&value, ty)?,
                _ => value,
            };
            named.push((parameter, value));
        }
        if positional.len() != positional_types.len() {
            return Err(binder_error(format!(
                "Function \"{name}\" expects {} positional argument{}, got {}",
                positional_types.len(),
                if positional_types.len() == 1 { "" } else { "s" },
                positional.len()
            )));
        }
        let (names, types): (Vec<String>, Vec<LogicalType>) = external_resource_columns(name)
            .into_iter()
            .map(|(n, t)| (n.to_string(), t))
            .unzip();
        // a body being defined runs nothing: C++ runs these functions when they execute, not when they bind
        if self.defining {
            return self.bind_rows(table_ref, name, names, types, Vec::new());
        }
        let Some(tables) = self
            .session
            .as_ref()
            .and_then(|session| session.external_resources.clone())
        else {
            return Err(Error::new(
                ErrorKind::NotImplemented,
                format!("{name} is not available here"),
            ));
        };
        match tables.call(name, positional, named)? {
            ExternalResourceRows::Rows(rows) => self.bind_rows(table_ref, name, names, types, rows),
            ExternalResourceRows::Query(sql) => {
                let select = super::table_functions::parse_select(&sql, "Expected a single SELECT statement")?;
                self.bind_rewritten(&select, table_ref, name)
            }
        }
    }
}
