// Ported from duckdb/src/main/external_resources_manager.cpp,
// duckdb/src/main/external_resource_type_registry.cpp,
// duckdb/src/function/table/system/external_resource_types.cpp,
// duckdb/src/function/table/system/external_resources.cpp and
// duckdb/src/function/table/system/create_external_resource.cpp @ rust-port-baseline
//
//! External resources: the kinds of resource the instance can provision and reap (`ExternalResourceTypeRegistry`),
//! the resources it manages (`ExternalResourcesManager`), and the table functions over them. The lifecycle
//! callbacks of a type are table functions or table macros named by the type; they run on an internal connection
//! of their own (`CreateExternalResourceCallbackConnection`), as the caller's statement is still running.

use crate::{ClientContext, DatabaseInstance};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_core::planner::binder::{ExternalResourceRows, ExternalResourceTables};
use parking_lot::Mutex;
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `ExternalResourceType`: a kind of resource, described by the names of its lifecycle callbacks.
#[derive(Debug, Clone, Default)]
pub struct ExternalResourceType {
    pub name: String,
    pub kind: String,
    pub create_function: String,
    pub status_function: String,
    pub destroy_function: String,
    pub resolve_function: String,
    pub list_function: String,
    /// "user" or "extension"
    pub origin: String,
    /// the search path of the registration, which the callbacks are resolved in
    pub search_path: String,
}

/// `ExternalResource`: a resource the instance manages, by its local name.
#[derive(Debug, Clone)]
pub struct ExternalResource {
    pub name: String,
    pub resource_type: String,
    pub handle: Value,
    pub uri: String,
    pub attached_db_type: String,
    pub deleter_function: String,
    pub deleter_payload: Value,
}

/// The type registry and the resources of an instance (shared by its connections, and not transactional: a
/// resource exists out in the world whether or not the transaction that registered it commits).
#[derive(Debug, Default)]
pub struct ExternalResources {
    types: Mutex<Vec<ExternalResourceType>>,
    resources: Mutex<Vec<ExternalResource>>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

impl ExternalResources {
    /// `ExternalResourceTypeRegistry::Add`: first wins.
    pub fn add_type(&self, resource_type: ExternalResourceType) -> Result<()> {
        if resource_type.name.is_empty() {
            return Err(invalid("an external resource type must have a name"));
        }
        if resource_type.kind.is_empty() {
            return Err(invalid(format!(
                "external resource type \"{}\": 'kind' is required",
                resource_type.name
            )));
        }
        if resource_type.create_function.is_empty() {
            return Err(invalid(format!(
                "external resource type \"{}\": 'create_function' is required",
                resource_type.name
            )));
        }
        // `ValidateFunctionName`: the callbacks are resolved by name only when they run
        for (key, value) in [
            ("create_function", &resource_type.create_function),
            ("status_function", &resource_type.status_function),
            ("destroy_function", &resource_type.destroy_function),
            ("resolve_function", &resource_type.resolve_function),
            ("list_function", &resource_type.list_function),
        ] {
            if (key == "create_function" || !value.is_empty())
                && let Err(error) = parse_name(value)
            {
                return Err(invalid(format!(
                    "external resource type \"{}\": '{key}' is not a valid function name: {error}",
                    resource_type.name
                )));
            }
        }
        let mut types = self.types.lock();
        if types.iter().any(|existing| existing.name == resource_type.name) {
            return Err(invalid(format!(
                "external resource type \"{}\" is already registered",
                resource_type.name
            )));
        }
        types.push(resource_type);
        Ok(())
    }

    pub fn types(&self) -> Vec<ExternalResourceType> {
        self.types.lock().clone()
    }

    pub fn lookup_type(&self, name: &str) -> Option<ExternalResourceType> {
        self.types.lock().iter().find(|t| t.name == name).cloned()
    }

    /// `ExternalResourcesManager::Add`: a name is claimed by its first registration.
    pub fn add(&self, resource: ExternalResource) -> Result<()> {
        if resource.name.is_empty() {
            return Err(invalid("an external resource instance must have a name"));
        }
        if resource.resource_type.is_empty() {
            return Err(invalid(format!(
                "external resource \"{}\": a type is required",
                resource.name
            )));
        }
        let mut resources = self.resources.lock();
        if resources.iter().any(|existing| existing.name == resource.name) {
            return Err(invalid(format!(
                "external resource \"{}\" is already registered",
                resource.name
            )));
        }
        resources.push(resource);
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Option<ExternalResource> {
        let mut resources = self.resources.lock();
        let at = resources.iter().position(|r| r.name == name)?;
        Some(resources.remove(at))
    }

    pub fn lookup(&self, name: &str) -> Option<ExternalResource> {
        self.resources.lock().iter().find(|r| r.name == name).cloned()
    }

    pub fn list(&self) -> Vec<ExternalResource> {
        self.resources.lock().clone()
    }
}

/// `MAX_EXTERNAL_RESOURCE_CALLBACK_DEPTH`: callbacks can call the external resource functions themselves.
const MAX_CALLBACK_DEPTH: usize = 16;
const DEFAULT_READINESS_TIMEOUT_SECONDS: i64 = 300;
const DEFAULT_POLL_INTERVAL_SECONDS: i64 = 5;
const MAX_READINESS_SECONDS: i64 = 30 * 24 * 60 * 60;

thread_local! {
    /// how deep the callback connections running on this thread are nested (`ClientContext::internal_depth`)
    static CALLBACK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// `MAP(VARCHAR, VARCHAR)`, the type of handles and status results.
pub fn handle_type() -> LogicalType {
    LogicalType::map(LogicalType::VARCHAR, LogicalType::VARCHAR)
}

/// An empty handle.
pub fn empty_map() -> Value {
    Value::new(handle_type(), ValueData::List(Vec::new())).expect("an empty list fits a MAP")
}

/// The entries of a MAP value, as key and value.
fn map_entries(value: &Value) -> Vec<(Value, Value)> {
    match value.data() {
        ValueData::List(items) => items
            .iter()
            .filter_map(|item| match item.data() {
                ValueData::Struct(fields) if fields.len() == 2 => Some((fields[0].clone(), fields[1].clone())),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `MapTryGet` of a `MAP(VARCHAR, VARCHAR)`.
fn map_get(value: &Value, key: &str) -> Option<String> {
    map_entries(value).into_iter().find_map(|(k, v)| {
        (!k.is_null() && k.try_to_string().ok().as_deref() == Some(key))
            .then(|| if v.is_null() { String::new() } else { v.try_to_string().unwrap_or_default() })
    })
}

fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `Value::ToSQLString` of what callbacks are given: handles, maps of parameters, and text.
pub fn value_sql(value: &Value) -> String {
    let ty = value.logical_type();
    if value.is_null() {
        return if ty.id() == LogicalTypeId::SqlNull {
            "NULL".to_string()
        } else {
            format!("NULL::{ty}")
        };
    }
    match ty.id() {
        LogicalTypeId::Map => {
            let entries: Vec<String> = map_entries(value)
                .iter()
                .map(|(k, v)| format!("{}: {}", value_sql(k), value_sql(v)))
                .collect();
            format!("MAP {{{}}}::{ty}", entries.join(", "))
        }
        LogicalTypeId::Varchar => sql_string(&value.try_to_string().unwrap_or_default()),
        _ => format!(
            "{}::{ty}",
            sql_string(&value.try_to_string().unwrap_or_default())
        ),
    }
}

/// `TryParseComponents`: the parts of a qualified name, `"quoted"` ones with their quotes doubled inside.
fn name_components(input: &str) -> std::result::Result<Vec<String>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut components = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let mut entry = String::new();
        if chars[at] == '"' {
            at += 1;
            let mut closed = false;
            while at < chars.len() {
                if chars[at] == '"' {
                    if chars.get(at + 1) == Some(&'"') {
                        entry.push('"');
                        at += 2;
                        continue;
                    }
                    at += 1;
                    closed = true;
                    break;
                }
                entry.push(chars[at]);
                at += 1;
            }
            if !closed {
                return Err(format!("Unterminated quote in qualified name! (input: {input})"));
            }
            if entry.is_empty() {
                return Err(format!(
                    "Zero-length delimited identifier in qualified name! (input: {input})"
                ));
            }
            if at < chars.len() && chars[at] != '.' {
                return Err(format!(
                    "Unexpected character after a quoted identifier in a qualified name! (input: {input})"
                ));
            }
        } else {
            while at < chars.len() && chars[at] != '.' {
                if chars[at] == '"' {
                    return Err(format!(
                        "Unexpected quote in the middle of a qualified name component! (input: {input})"
                    ));
                }
                entry.push(chars[at]);
                at += 1;
            }
        }
        components.push(entry);
        if at < chars.len() {
            at += 1;
        }
    }
    Ok(components)
}

/// `QualifiedName::TryParse`: the components, none of them empty.
fn parse_name(input: &str) -> std::result::Result<Vec<String>, String> {
    let components = name_components(input)?;
    if components.is_empty() || components.iter().any(String::is_empty) {
        return Err(format!("Zero-length identifier in qualified name! (input: {input})"));
    }
    Ok(components)
}

/// `QualifiedName::Parse(name).ToString()`: each part of a (maybe qualified) callback name quoted as it needs.
fn callback_sql(name: &str) -> String {
    let components = name_components(name).unwrap_or_else(|_| vec![name.to_string()]);
    components
        .iter()
        .map(|part| duckrs_parser::ast::sql_identifier(part))
        .collect::<Vec<_>>()
        .join(".")
}

/// `RequireResourceMap`: what a callback returned in `column`, as a `MAP(VARCHAR, VARCHAR)`.
fn require_map(value: Value, function: &str, column: &str) -> Result<Value> {
    if value.is_null() {
        return Ok(value);
    }
    if value.logical_type().id() != LogicalTypeId::Map {
        return Err(invalid(format!(
            "external resource callback \"{function}\" must return a MAP in its '{column}' column, got {}",
            value.logical_type()
        )));
    }
    duckrs_core::function::cast::cast_value(&value, &handle_type())
}

/// The external resource table functions as a statement of one connection sees them.
pub struct ExternalResourceView {
    pub db: Arc<DatabaseInstance>,
    /// the logger of the calling connection
    pub log: Option<Arc<duckrs_core::logging::LogTarget>>,
    /// the search path that was set (`GetSetPaths`), as text
    pub search_path: String,
}

/// `CreateExternalResourceCallbackConnection`: a connection of its own, one callback level deeper.
pub struct CallbackConnection {
    context: ClientContext,
}

impl CallbackConnection {
    pub fn open(db: &Arc<DatabaseInstance>) -> Result<Self> {
        if CALLBACK_DEPTH.with(Cell::get) + 1 > MAX_CALLBACK_DEPTH {
            return Err(invalid(format!(
                "External resource callbacks are nested too deeply (maximum depth {MAX_CALLBACK_DEPTH}) - does a \
                 callback call itself recursively?"
            )));
        }
        let mut context = ClientContext::new(db.clone());
        context.nested = true;
        Ok(Self { context })
    }

    pub fn query(&mut self, sql: &str) -> Result<crate::MaterializedQueryResult> {
        CALLBACK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        let result = self.context.query(sql);
        CALLBACK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        result
    }

    /// `QualifyTableCallback`: the table function or table macro `name` as `catalog.schema.name`, in the search
    /// path of this connection.
    pub fn qualify(&mut self, name: &str) -> Option<String> {
        if name_components(name).is_ok_and(|parts| parts.len() > 1) {
            return Some(name.to_string());
        }
        let found = self
            .query(&format!(
                "SELECT database_name, schema_name FROM duckdb_functions() WHERE function_name = {} AND \
                 function_type IN ('table', 'table_macro') AND schema_name IN (SELECT unnest(current_schemas(true))) \
                 ORDER BY list_position(current_schemas(true), schema_name), internal LIMIT 1",
                sql_string(name)
            ))
            .ok()?;
        if found.row_count() == 0 {
            return None;
        }
        let (database, schema) = (
            found.value(0, 0).try_to_string().ok()?,
            found.value(1, 0).try_to_string().ok()?,
        );
        Some([database.as_str(), schema.as_str(), name].map(duckrs_parser::ast::sql_identifier).join("."))
    }
}

fn text(value: &Value) -> Option<String> {
    (!value.is_null()).then(|| value.try_to_string().unwrap_or_default())
}

fn nullable(text: &str) -> Value {
    if text.is_empty() {
        Value::null(LogicalType::VARCHAR)
    } else {
        Value::varchar(text)
    }
}

impl ExternalResourceView {
    /// `LogExternalResourceOperation`: the outcome of one operation (`error` is that of the operation, not only
    /// of its callback query).
    fn log<T>(&self, resource_type: &str, operation: &str, outcome: &Result<T>, extra: &[(&str, &str)]) {
        let error = outcome.as_ref().err().map(|e| e.message.clone()).unwrap_or_default();
        duckrs_core::logging::log_external_resource(self.log.as_ref(), resource_type, "", operation, &error, extra);
    }

    fn registry(&self) -> &ExternalResources {
        &self.db.external_resources
    }

    /// A callback connection in the search path the type was registered with (or the caller's).
    fn connection(&self, search_path: &str) -> Result<CallbackConnection> {
        let mut connection = CallbackConnection::open(&self.db)?;
        if !search_path.is_empty() {
            connection.query(&format!("SET search_path = {}", sql_string(search_path)))?;
        }
        Ok(connection)
    }

    fn register_type(&self, positional: &[Value], named: &[(String, Value)]) -> Result<ExternalResourceRows> {
        let name = text(&positional[0]).filter(|name| !name.is_empty()).ok_or_else(|| {
            invalid("register_external_resource_type: name must not be NULL or empty")
        })?;
        let mut resource_type = ExternalResourceType {
            name,
            origin: "user".into(),
            search_path: self.search_path.clone(),
            ..Default::default()
        };
        for (key, value) in named {
            let value = text(value).unwrap_or_default();
            match key.as_str() {
                "kind" => resource_type.kind = value,
                "create_function" => resource_type.create_function = value,
                "status_function" => resource_type.status_function = value,
                "destroy_function" => resource_type.destroy_function = value,
                "resolve_function" => resource_type.resolve_function = value,
                "list_function" => resource_type.list_function = value,
                _ => {}
            }
        }
        self.registry().add_type(resource_type)?;
        Ok(ExternalResourceRows::Rows(vec![vec![Value::boolean(true)]]))
    }

    fn types(&self) -> ExternalResourceRows {
        let rows = self
            .registry()
            .types()
            .into_iter()
            .map(|t| {
                vec![
                    Value::varchar(t.name),
                    Value::varchar(t.kind),
                    Value::varchar(t.create_function),
                    nullable(&t.status_function),
                    nullable(&t.destroy_function),
                    nullable(&t.resolve_function),
                    nullable(&t.list_function),
                    Value::varchar(t.origin),
                ]
            })
            .collect();
        ExternalResourceRows::Rows(rows)
    }

    fn register(&self, positional: &[Value], named: &[(String, Value)]) -> Result<ExternalResourceRows> {
        if positional[0].is_null() || positional[1].is_null() {
            return Err(invalid("register_external_resource: the type and name must not be NULL"));
        }
        if positional[2].is_null() {
            return Err(invalid("register_external_resource: the handle must not be NULL"));
        }
        let mut resource = ExternalResource {
            resource_type: text(&positional[0]).unwrap_or_default(),
            name: text(&positional[1]).unwrap_or_default(),
            handle: positional[2].clone(),
            uri: String::new(),
            attached_db_type: String::new(),
            deleter_function: String::new(),
            deleter_payload: positional[2].clone(),
        };
        for (key, value) in named {
            let Some(value) = text(value) else { continue };
            match key.as_str() {
                "uri" => resource.uri = value,
                "attached_db_type" => resource.attached_db_type = value,
                // qualified in the caller's search path: teardown runs on another connection
                "deleter_function" => {
                    let qualified = self.connection(&self.search_path)?.qualify(&value);
                    resource.deleter_function = qualified.unwrap_or(value);
                }
                _ => {}
            }
        }
        self.registry().add(resource)?;
        Ok(ExternalResourceRows::Rows(vec![vec![Value::boolean(true)]]))
    }

    fn deregister(&self, positional: &[Value]) -> Result<ExternalResourceRows> {
        let name = text(&positional[0]).filter(|name| !name.is_empty()).ok_or_else(|| {
            invalid("deregister_external_resource: the name must not be NULL or empty")
        })?;
        if self.registry().remove(&name).is_none() {
            return Err(invalid(format!(
                "deregister_external_resource: \"{name}\" is not registered"
            )));
        }
        Ok(ExternalResourceRows::Rows(vec![vec![Value::boolean(true)]]))
    }

    /// `duckdb_external_resources`: the registry, and with `discover` what the list callbacks find.
    fn resources(&self, named: &[(String, Value)]) -> Result<ExternalResourceRows> {
        let mut discover = false;
        let mut wanted = String::new();
        for (key, value) in named {
            if value.is_null() {
                continue;
            }
            match key.as_str() {
                "discover" => discover = value.try_to_string()? == "true",
                "type" => wanted = value.try_to_string()?,
                _ => {}
            }
        }
        let matches = |name: &str| wanted.is_empty() || wanted == name;
        if !discover {
            let rows = self
                .registry()
                .list()
                .into_iter()
                .filter(|r| matches(&r.resource_type))
                .map(|r| {
                    vec![
                        Value::varchar(r.name),
                        Value::varchar(r.resource_type),
                        Value::boolean(true),
                        Value::null(LogicalType::VARCHAR),
                        Value::null(LogicalType::VARCHAR),
                        nullable(&r.uri),
                        nullable(&r.attached_db_type),
                        if r.handle.is_null() { empty_map() } else { r.handle },
                    ]
                })
                .collect();
            return Ok(ExternalResourceRows::Rows(rows));
        }
        let registry_scan = |name: &str| {
            if name.is_empty() {
                "duckdb_external_resources()".to_string()
            } else {
                format!("duckdb_external_resources(type := {})", sql_string(name))
            }
        };
        let mut branches = vec![format!(
            "SELECT name, type, managed, NULL::VARCHAR AS state, NULL::VARCHAR AS reference, uri, attached_db_type, \
             handle FROM {}",
            registry_scan(&wanted)
        )];
        let mut matched = false;
        for resource_type in self.registry().types() {
            if !matches(&resource_type.name) {
                continue;
            }
            matched = true;
            if resource_type.list_function.is_empty() {
                continue;
            }
            let Some(callback) = self.connection(&self.search_path)?.qualify(&resource_type.list_function) else {
                return Err(invalid(format!(
                    "external resource discovery for type \"{}\": the list callback \"{}\" does not exist",
                    resource_type.name, resource_type.list_function
                )));
            };
            // (`KeywordHelper::WriteQuotedAndEscaped`: always quoted)
            let alias = format!("\"{}\"", resource_type.list_function.replace('"', "\"\""));
            branches.push(format!(
                "SELECT NULL::VARCHAR AS name, {type_name} AS type, false AS managed, {alias}.state::VARCHAR AS state, \
                 {alias}.reference::VARCHAR AS reference, NULL::VARCHAR AS uri, NULL::VARCHAR AS attached_db_type, \
                 {alias}.handle::MAP(VARCHAR, VARCHAR) AS handle FROM {callback}(MAP {{}}::MAP(VARCHAR, VARCHAR)) AS \
                 {alias} ANTI JOIN {registry} AS registry ON {alias}.handle = registry.handle",
                type_name = sql_string(&resource_type.name),
                registry = registry_scan(&resource_type.name),
            ));
        }
        let known = self.registry().list().iter().any(|r| r.resource_type == wanted);
        if !matched && !wanted.is_empty() && !known {
            return Err(invalid(format!(
                "duckdb_external_resources: unknown resource type \"{wanted}\""
            )));
        }
        Ok(ExternalResourceRows::Query(branches.join(" UNION ALL ")))
    }

    fn destroy(&self, positional: &[Value]) -> Result<ExternalResourceRows> {
        let function = text(&positional[0]).filter(|f| !f.is_empty()).ok_or_else(|| {
            invalid("destroy_external_resource: the deleter function must not be NULL or empty")
        })?;
        let sql = format!(
            "SELECT * FROM {}({})",
            callback_sql(&function),
            value_sql(&positional[1])
        );
        let mut connection = CallbackConnection::open(&self.db)?;
        let outcome = connection.query(&sql).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("destroy_external_resource: deleter function \"{function}\" failed: {error}"),
            )
        });
        // (the deleter binding does not say which resource it is of)
        self.log("", "destroy", &outcome, &[]);
        outcome?;
        Ok(ExternalResourceRows::Rows(vec![vec![Value::boolean(true)]]))
    }

    /// `create_external_resource`: create (or adopt) the resource, and poll its status until it is ready.
    fn create(&self, positional: &[Value], named: &[(String, Value)]) -> Result<ExternalResourceRows> {
        let type_name = text(&positional[0])
            .ok_or_else(|| invalid("create_external_resource: the type name must not be NULL"))?;
        let mut params = empty_map();
        let mut adopt = None;
        let mut teardown_on_failure = true;
        let mut timeout = DEFAULT_READINESS_TIMEOUT_SECONDS;
        let mut poll_interval = DEFAULT_POLL_INTERVAL_SECONDS;
        let seconds = |key: &str, value: &Value, minimum: i64| -> Result<i64> {
            let seconds: i64 = value.try_to_string()?.parse().unwrap_or(i64::MAX);
            if seconds < minimum || seconds > MAX_READINESS_SECONDS {
                return Err(invalid(format!(
                    "create_external_resource: '{key}' must be between {minimum} and {MAX_READINESS_SECONDS}, got \
                     {seconds}"
                )));
            }
            Ok(seconds)
        };
        for (key, value) in named {
            if value.is_null() {
                continue;
            }
            match key.as_str() {
                "params" => params = value.clone(),
                "handle" => {
                    adopt = Some(if value.logical_type().id() == LogicalTypeId::Map {
                        value.clone()
                    } else {
                        duckrs_core::function::cast::cast_value(value, &handle_type())?
                    })
                }
                "teardown_on_failure" => teardown_on_failure = value.try_to_string()? == "true",
                "timeout_seconds" => timeout = seconds(key, value, 0)?,
                "poll_interval_seconds" => poll_interval = seconds(key, value, 1)?,
                _ => {}
            }
        }
        let Some(resource_type) = self.registry().lookup_type(&type_name) else {
            return Err(invalid(format!(
                "create_external_resource: unknown resource type \"{type_name}\""
            )));
        };
        let mut connection = self.connection(&resource_type.search_path)?;
        let adopting = adopt.is_some();
        let handle = match adopt {
            Some(handle) => require_map(handle, &type_name, "handle")?,
            None => {
                let function = &resource_type.create_function;
                let sql = format!("SELECT * FROM {}({})", callback_sql(function), value_sql(&params));
                let mut provision = || -> Result<Value> {
                    let result = connection.query(&sql).map_err(|error| {
                        Error::new(
                            ErrorKind::Io,
                            format!("create_external_resource: create function \"{function}\" failed: {error}"),
                        )
                    })?;
                    if result.row_count() == 0 {
                        return Err(invalid(format!(
                            "create_external_resource: create function \"{function}\" returned no rows"
                        )));
                    }
                    let handle = require_map(result.value(0, 0), function, "handle")?;
                    if handle.is_null() {
                        return Err(invalid(format!(
                            "create_external_resource: create function \"{function}\" returned a NULL handle"
                        )));
                    }
                    Ok(handle)
                };
                let outcome = provision();
                self.log(&type_name, "create", &outcome, &[]);
                outcome?
            }
        };
        // what was provisioned here is torn down if it does not get to be ready
        let teardown = !adopting && teardown_on_failure;
        let ready = self.await_ready(&mut connection, &resource_type, &type_name, &handle, timeout, poll_interval);
        let status = match ready {
            Ok(status) => status,
            Err(error) => {
                if teardown && !resource_type.destroy_function.is_empty() {
                    let sql = format!(
                        "SELECT * FROM {}({})",
                        callback_sql(&resource_type.destroy_function),
                        value_sql(&handle)
                    );
                    let outcome = connection.query(&sql);
                    self.log(&type_name, "destroy", &outcome, &[]);
                }
                return Err(error);
            }
        };
        let uri = map_get(&status, "uri");
        let attached_db_type = map_get(&status, "attached_db_type");
        if resource_type.kind == "catalog" {
            let missing = |name: &str| {
                invalid(format!(
                    "create_external_resource: catalog resource \"{type_name}\" is missing '{name}' in its status \
                     result"
                ))
            };
            if uri.as_deref().is_none_or(str::is_empty) {
                return Err(missing("uri"));
            }
            if attached_db_type.as_deref().is_none_or(str::is_empty) {
                return Err(missing("attached_db_type"));
            }
        }
        let mut deleter = resource_type.destroy_function.clone();
        if !deleter.is_empty()
            && !resource_type.search_path.is_empty()
            && let Some(qualified) = connection.qualify(&deleter)
        {
            deleter = qualified;
        }
        let varchar = |text: Option<String>| text.map_or_else(|| Value::null(LogicalType::VARCHAR), Value::varchar);
        Ok(ExternalResourceRows::Rows(vec![vec![
            handle.clone(),
            varchar(uri),
            varchar(attached_db_type),
            status,
            nullable(&deleter),
            handle,
        ]]))
    }

    /// The blocking poll of `status(handle)` until its state is 'ready' or 'failed'.
    fn await_ready(
        &self,
        connection: &mut CallbackConnection,
        resource_type: &ExternalResourceType,
        type_name: &str,
        handle: &Value,
        timeout: i64,
        poll_interval: i64,
    ) -> Result<Value> {
        let function = &resource_type.status_function;
        if function.is_empty() {
            return Err(invalid(format!(
                "create_external_resource: resource type \"{type_name}\" has no status function (required to \
                 await readiness)"
            )));
        }
        let deadline = (timeout > 0).then(|| Instant::now() + Duration::from_secs(timeout as u64));
        loop {
            duckrs_core::execution::interrupt::check()?;
            let sql = format!("SELECT state, result FROM {}({})", callback_sql(function), value_sql(handle));
            let mut state = String::new();
            let outcome = self.poll(connection, function, type_name, &sql, deadline, &mut state);
            let extra = [("state", state.as_str())];
            self.log(type_name, "status", &outcome, if state.is_empty() { &[] } else { &extra });
            if let Some(result) = outcome? {
                return Ok(result);
            }
            // an interruptible wait: the deadline of the query is checked between short sleeps
            let until = Instant::now() + Duration::from_secs(poll_interval as u64);
            while Instant::now() < until {
                duckrs_core::execution::interrupt::check()?;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// One poll of the status: the result once ready, `None` while it is not.
    fn poll(
        &self,
        connection: &mut CallbackConnection,
        function: &str,
        type_name: &str,
        sql: &str,
        deadline: Option<Instant>,
        observed: &mut String,
    ) -> Result<Option<Value>> {
        {
            let result = connection.query(sql).map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!(
                        "create_external_resource: status function \"{function}\" failed: {}",
                        error
                    ),
                )
            })?;
            if result.row_count() == 0 {
                return Err(invalid(format!(
                    "create_external_resource: status function \"{function}\" returned no rows"
                )));
            }
            let state = text(&result.value(0, 0)).unwrap_or_default();
            observed.clone_from(&state);
            if state == "failed" {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("create_external_resource: resource \"{type_name}\" reported state 'failed'"),
                ));
            }
            if state == "ready" {
                return require_map(result.value(1, 0), function, "result").map(Some);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "create_external_resource: timed out awaiting readiness for \"{type_name}\" (last state \
                         '{state}')"
                    ),
                ));
            }
            Ok(None)
        }
    }
}

impl ExternalResourceTables for ExternalResourceView {
    fn call(
        &self,
        function: &str,
        positional: Vec<Value>,
        named: Vec<(String, Value)>,
    ) -> Result<ExternalResourceRows> {
        match function {
            "register_external_resource_type" => self.register_type(&positional, &named),
            "duckdb_external_resource_types" => Ok(self.types()),
            "register_external_resource" => self.register(&positional, &named),
            "deregister_external_resource" => self.deregister(&positional),
            "duckdb_external_resources" => self.resources(&named),
            "create_external_resource" => self.create(&positional, &named),
            "destroy_external_resource" => self.destroy(&positional),
            _ => Err(duckrs_core::unported(function)),
        }
    }
}
