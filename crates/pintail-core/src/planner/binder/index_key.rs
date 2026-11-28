// Ported from duckdb/extension/core_functions/scalar/debug/index_key.cpp @ rust-port-baseline
//
//! `index_key(path, keys...)`: the ART key that an index of a table makes of the key values.

use super::{Binder, binder_error};
use crate::catalog::{Catalog, TableConstraint};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::BoundExpr;
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};
use pintail_parser::ast::{FunctionExpr, sql_quoted_identifier};
use std::sync::Arc;

/// Which index of the table: a user index by name, or the index of a constraint.
enum IndexChoice {
    Named(String),
    Constraint { kind: &'static str, columns: Vec<String> },
}

/// `EvaluateIndexKeyPath`.
fn evaluate_path(path: &Value) -> Result<(Option<String>, String, String, IndexChoice)> {
    let field_names: Vec<String> = path
        .logical_type()
        .struct_children()
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let ValueData::Struct(values) = path.data() else {
        return Err(binder_error("index_key: path parameter cannot be NULL"));
    };
    let (mut catalog, mut schema, mut table, mut index, mut constraint) = (None, None, None, None, None);
    let mut columns = Vec::new();
    for (name, value) in field_names.iter().zip(values) {
        let quoted = sql_quoted_identifier(name);
        if value.is_null() {
            return Err(binder_error(format!("index_key: path field {quoted} cannot be NULL")));
        }
        let lower = name.to_lowercase();
        if lower == "columns" {
            let ty = value.logical_type();
            if ty.id() != LogicalTypeId::List || ty.child_type().is_none_or(|c| c.id() != LogicalTypeId::Varchar) {
                return Err(binder_error(format!(
                    "index_key: path field {quoted} must be VARCHAR[]"
                )));
            }
            let ValueData::List(items) = value.data() else {
                unreachable!("a list")
            };
            for item in items {
                if item.is_null() {
                    return Err(binder_error(format!(
                        "index_key: path field {quoted} cannot contain NULL"
                    )));
                }
                columns.push(item.try_to_string()?);
            }
            continue;
        }
        let slot = match lower.as_str() {
            "catalog" => &mut catalog,
            "schema" => &mut schema,
            "table" => &mut table,
            "index" => &mut index,
            "constraint" => &mut constraint,
            _ => return Err(binder_error(format!("index_key: unknown field {quoted} in path"))),
        };
        if value.logical_type().id() != LogicalTypeId::Varchar {
            return Err(binder_error(format!("index_key: path field {quoted} must be VARCHAR")));
        }
        let text = value.try_to_string()?;
        if text.is_empty() {
            return Err(binder_error(format!("index_key: path field {quoted} cannot be empty")));
        }
        *slot = Some(text);
    }
    let Some(table) = table else {
        return Err(binder_error("index_key: path must contain a 'table' field"));
    };
    let choice = match (index, constraint) {
        (Some(index), None) => {
            if !columns.is_empty() {
                return Err(binder_error(
                    "index_key: path field 'columns' requires a 'constraint' field",
                ));
            }
            IndexChoice::Named(index)
        }
        (None, Some(constraint)) => {
            let kind = match constraint.to_lowercase().as_str() {
                "primary" => "PRIMARY",
                "unique" => "UNIQUE",
                "foreign" => "FOREIGN",
                _ => {
                    return Err(binder_error(
                        "index_key: constraint must be one of 'primary', 'unique' or 'foreign'",
                    ));
                }
            };
            IndexChoice::Constraint { kind, columns }
        }
        _ => {
            return Err(binder_error(
                "index_key: path must contain exactly one of the 'index' or 'constraint' fields",
            ));
        }
    };
    Ok((catalog, schema.unwrap_or_else(|| "main".to_string()), table, choice))
}

/// The ART keys of the rows of the key columns, NULL where a key has a NULL (`GenerateKeys`).
fn index_key(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let count = args.len();
    let columns: Vec<&Vector> = args.columns().iter().collect();
    let keys = crate::execution::art::key::chunk_keys(&columns, count)?;
    for (row, key) in keys.into_iter().enumerate() {
        let value = match key {
            Some(key) if !key.is_empty() => Value::blob(key.bytes().to_vec()),
            _ => Value::null(LogicalType::BLOB),
        };
        result.set_value(row, &value)?;
    }
    Ok(())
}

impl Binder<'_> {
    /// `IndexKeyBind`.
    pub(super) fn bind_index_key(&mut self, function: &FunctionExpr) -> Result<BoundExpr> {
        let no_match = |types: &[String]| {
            binder_error(format!(
                "No function matches the given name and argument types 'index_key({})'. You might need to add \
                 explicit type casts.",
                types.join(", ")
            ))
        };
        let arguments = function
            .args
            .iter()
            .map(|a| self.bind_expression(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        let types: Vec<String> = arguments
            .iter()
            .map(|a| crate::function::binder::expression_return_type(a).to_string())
            .collect();
        let Some(path) = arguments.first() else {
            return Err(no_match(&types));
        };
        if !matches!(path.return_type.id(), LogicalTypeId::Struct | LogicalTypeId::SqlNull) {
            return Err(no_match(&types));
        }
        if !path.is_foldable() {
            return Err(binder_error(
                "The path argument in function index_key must be a constant expression",
            ));
        }
        let path = evaluate_scalar(path)?;
        // the default NULL handling: a NULL argument is a NULL result
        if path.is_null() {
            return Ok(BoundExpr::constant(Value::null(LogicalType::BLOB)));
        }
        let (catalog_name, schema, table_name, choice) = evaluate_path(&path)?;
        let current: Arc<Catalog>;
        let catalog: &Catalog = match &catalog_name {
            Some(name)
                if !self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.database.eq_ignore_ascii_case(name))
                    && !name.eq_ignore_ascii_case(&self.catalog.name) =>
            {
                current = self
                    .session
                    .as_ref()
                    .and_then(|session| (session.attached_catalog)(name))
                    .ok_or_else(|| binder_error(format!("Catalog {} does not exist!", sql_quoted_identifier(name))))?;
                &current
            }
            _ => self.catalog,
        };
        let table = catalog.get_table(Some(&schema), &table_name)?;
        let qualified = if schema.eq_ignore_ascii_case("main") {
            table_name.clone()
        } else {
            format!("{schema}.{table_name}")
        };
        let (key_types, description) = match &choice {
            IndexChoice::Named(name) => {
                let indexes = table.indexes.read();
                let Some(index) = indexes.iter().find(|i| i.name.eq_ignore_ascii_case(name)) else {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!(
                            "index_key: index {} was not found on table {qualified}",
                            sql_quoted_identifier(name)
                        ),
                    ));
                };
                let types: Vec<LogicalType> = index.expressions.iter().map(|e| e.return_type.clone()).collect();
                (types, format!("index {}", sql_quoted_identifier(name)))
            }
            IndexChoice::Constraint { kind, columns } => {
                let mut found: Vec<Vec<usize>> = Vec::new();
                for constraint in &table.constraints {
                    let (constraint_kind, keys) = match constraint {
                        TableConstraint::Unique {
                            columns, primary_key, ..
                        } => (if *primary_key { "PRIMARY" } else { "UNIQUE" }, columns.clone()),
                        TableConstraint::ForeignKey(key) => ("FOREIGN", key.columns.clone()),
                        TableConstraint::Check { .. } | TableConstraint::NotNull(_) => continue,
                    };
                    let names: Vec<&str> = keys.iter().map(|&k| table.columns[k].name.as_str()).collect();
                    let matches = columns.is_empty()
                        || (columns.len() == names.len()
                            && columns.iter().zip(&names).all(|(a, b)| a.eq_ignore_ascii_case(b)));
                    if constraint_kind == *kind && matches {
                        found.push(keys);
                    }
                }
                let keys = match found.as_slice() {
                    [] => {
                        return Err(Error::new(
                            ErrorKind::Catalog,
                            format!("index_key: {kind} constraint was not found on table {qualified}"),
                        ));
                    }
                    [keys] => keys.clone(),
                    _ => {
                        return Err(binder_error(format!(
                            "index_key: table {qualified} has multiple {kind} constraints, specify 'columns' to \
                             select one"
                        )));
                    }
                };
                let types = keys.iter().map(|&k| table.columns[k].ty.clone()).collect();
                (types, format!("{kind} constraint index"))
            }
        };
        let given = arguments.len() - 1;
        if given != key_types.len() {
            return Err(binder_error(format!(
                "index_key: {description} on table {qualified} expects {} key column(s), but {given} argument(s) \
                 provided",
                key_types.len()
            )));
        }
        let keys = arguments
            .into_iter()
            .skip(1)
            .zip(&key_types)
            .map(|(argument, ty)| super::expression::checked_cast(argument, ty))
            .collect::<Result<Vec<_>>>()?;
        Ok(crate::function::nested::bound_function(
            "index_key",
            keys,
            LogicalType::BLOB,
            index_key,
        ))
    }
}
