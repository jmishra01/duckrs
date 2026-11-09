// Ported from duckdb/src/function/cast/variant/from_variant.cpp and
// duckdb/src/function/scalar/variant/{variant_extract,variant_typeof,variant_keys,
// variant_array_length}.cpp @ rust-port-baseline
//
//! Casts from VARIANT and the functions on it, value by value.

use super::cast::{CastMode, conversion_error};
use super::nested::{bound_function, row_wise};
use super::nested_casts::cast_value;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::variant::VariantNode;
use duckrs_common::vector::{DataChunk, Vector, VectorType};
use duckrs_common::{Error, ErrorKind, Result};

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

pub(super) fn variant_type() -> LogicalType {
    LogicalType::new(LogicalTypeId::Variant)
}

/// A VARIANT value of `node`.
fn variant_value(node: &VariantNode) -> Value {
    Value::new(variant_type(), ValueData::Bytes(node.encode())).expect("a VARIANT value")
}

/// `CastToVARIANT` for one value: a NULL value stays a SQL NULL.
pub(super) fn to_variant(value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::null(variant_type()));
    }
    Ok(variant_value(&VariantNode::from_value(value)?))
}

fn decode(value: &Value) -> Result<VariantNode> {
    match value.data() {
        ValueData::Bytes(bytes) => VariantNode::decode(bytes),
        _ => unreachable!("a VARIANT is stored as bytes"),
    }
}

/// The message of a failure; the top level adds the target type.
type Conversion<T> = std::result::Result<T, Failure>;

enum Failure {
    /// DuckDB's sentence, which ends in "to '<target>'" at the top.
    Message(String),
    Error(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Error(error)
    }
}

fn expected(kind: &str, node: &VariantNode) -> Failure {
    Failure::Message(format!(
        "Expected to find VARIANT({kind}), found VARIANT({}) instead, can't convert",
        node.type_name()
    ))
}

/// `CastVariant`.
fn convert(node: &VariantNode, target: &LogicalType) -> Conversion<Value> {
    use LogicalTypeId as L;
    if matches!(node, VariantNode::Null) {
        return Ok(Value::null(target.clone()));
    }
    // a string can hold the VARCHAR form of a nested value
    if let VariantNode::Scalar(value) = node
        && target.is_nested()
        && value.logical_type().id() == L::Varchar
        && let Ok(cast) = cast_value(value, target, CastMode::Strict)
    {
        return Ok(cast);
    }
    match target.id() {
        L::Variant => Ok(variant_value(node)),
        L::Varchar => Ok(Value::varchar(node.render()?)),
        L::List | L::Array => {
            let VariantNode::Array(items) = node else {
                return Err(expected("ARRAY", node));
            };
            let child = target.child_type().expect("a child type");
            if let Some(size) = target.array_size().filter(|_| target.id() == L::Array)
                && items.len() != size as usize
            {
                return Err(Failure::Message(format!(
                    "Array size '{size}' was expected, found '{}', can't convert VARIANT",
                    items.len()
                )));
            }
            let items = items
                .iter()
                .map(|n| convert(n, child))
                .collect::<Conversion<Vec<_>>>()?;
            Ok(Value::new(target.clone(), ValueData::List(items))?)
        }
        L::Tuple => {
            let VariantNode::Array(items) = node else {
                return Err(expected("ARRAY", node));
            };
            let mut fields = Vec::new();
            for (i, (_, ty)) in target.struct_children().iter().enumerate() {
                let Some(item) = items.get(i) else {
                    return Err(Failure::Message(format!(
                        "VARIANT(ARRAY) is missing element at index {i}, can't convert to TUPLE"
                    )));
                };
                fields.push(convert(item, ty)?);
            }
            Ok(Value::new(target.clone(), ValueData::Struct(fields))?)
        }
        L::Struct => {
            let VariantNode::Object(children) = node else {
                return Err(expected("OBJECT", node));
            };
            let mut fields = Vec::new();
            for (name, ty) in target.struct_children() {
                let Some((_, child)) = children.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) else {
                    let keys: Vec<&str> = children.iter().map(|(k, _)| k.as_str()).collect();
                    return Err(Failure::Message(format!(
                        "VARIANT(OBJECT({})) is missing key '{name}'",
                        keys.join(",")
                    )));
                };
                fields.push(convert(child, ty)?);
            }
            Ok(Value::new(target.clone(), ValueData::Struct(fields))?)
        }
        L::Map => {
            // a MAP is an ARRAY of key and value OBJECTs
            let entry = target.child_type().expect("an entry type");
            let list = convert(node, &LogicalType::list(entry.clone()))?;
            let ValueData::List(items) = list.data() else {
                unreachable!("a list value");
            };
            for (i, item) in items.iter().enumerate() {
                let ValueData::Struct(fields) = item.data() else {
                    return Err(expected("OBJECT", node));
                };
                if fields[0].is_null() {
                    return Err(Failure::Error(conversion_error(
                        "Map keys can not be NULL.".to_string(),
                    )));
                }
                let same_key = |other: &Value| matches!(other.data(), ValueData::Struct(f) if f[0] == fields[0]);
                if items[..i].iter().any(same_key) {
                    return Err(Failure::Error(Error::new(
                        ErrorKind::InvalidInput,
                        "Map keys must be unique.",
                    )));
                }
            }
            Ok(Value::new(target.clone(), list.data().clone())?)
        }
        _ => match node {
            VariantNode::Scalar(value) => {
                let cast = cast_value(value, target, CastMode::Try)?;
                if cast.is_null() {
                    return Err(Failure::Message(format!(
                        "Can't convert VARIANT({}) value '{}'",
                        node.type_name(),
                        value.try_to_string()?
                    )));
                }
                Ok(cast)
            }
            other => Err(Failure::Message(format!(
                "Can't convert VARIANT({}) value '{}'",
                other.type_name(),
                other.render()?
            ))),
        },
    }
}

/// A VARIANT value as `target`.
pub(super) fn from_variant(value: &Value, target: &LogicalType) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::null(target.clone()));
    }
    match convert(&decode(value)?, target) {
        Ok(value) => Ok(value),
        Err(Failure::Error(error)) => Err(error),
        Err(Failure::Message(message)) => Err(conversion_error(format!("{message} to '{target}'"))),
    }
}

/// Casts from or to VARIANT, row by row.
pub(super) fn variant_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let target = result.logical_type().clone();
    let constant = source.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    result.prepare_flat(rows.max(1));
    for i in 0..rows {
        let value = source.get_value(i);
        let cast = if target.id() == LogicalTypeId::Variant {
            to_variant(&value)
        } else {
            from_variant(&value, &target)
        };
        match cast {
            Ok(cast) => result.set_value(i, &cast)?,
            Err(_) if mode == CastMode::Try => result.set_null(i, true),
            Err(error) => return Err(error),
        }
    }
    if constant {
        result.set_constant();
    }
    Ok(())
}

fn variant_typeof(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::varchar("VARIANT_NULL"));
        }
        Ok(Value::varchar(decode(&row[0])?.describe()))
    })
}

/// One step of `variant_extract`: a child, or nothing.
fn extract_child(node: &VariantNode, key: &Value) -> Option<VariantNode> {
    match (node, key.data()) {
        (VariantNode::Object(children), ValueData::Bytes(key)) => {
            let key = String::from_utf8_lossy(key);
            children
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, c)| c.clone())
        }
        (VariantNode::Array(items), _) => {
            let index = key.as_i128().filter(|i| *i >= 1)?;
            items.get(index as usize - 1).cloned()
        }
        _ => None,
    }
}

fn variant_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(variant_type()));
        }
        Ok(match extract_child(&decode(&row[0])?, &row[1]) {
            // a NULL inside of a VARIANT is NULL once it is taken out
            Some(VariantNode::Null) | None => Value::null(variant_type()),
            Some(node) => variant_value(&node),
        })
    })
}

/// `variant_keys`, of the value or of a child: the sorted keys of an object.
fn variant_keys(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let mut node = decode(&row[0])?;
        if let Some(key) = row.get(1)
            && !key.is_null()
        {
            let Some(child) = extract_child(&node, key) else {
                return Ok(Value::null(ty.clone()));
            };
            node = child;
        }
        let keys = match node {
            VariantNode::Object(children) => children.into_iter().map(|(k, _)| Value::varchar(k)).collect(),
            _ => Vec::new(),
        };
        Value::list(LogicalType::VARCHAR, keys)
    })
}

fn variant_array_length(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::BIGINT));
        }
        Ok(match decode(&row[0])? {
            VariantNode::Array(items) => Value::bigint(items.len() as i64),
            _ => Value::null(LogicalType::BIGINT),
        })
    })
}

/// Whether [`bind_variant_function`] binds the function `name`.
pub fn is_variant_function(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "variant_typeof"
            | "variant_extract"
            | "variant_keys"
            | "variant_array_length"
            | "variant_to_parquet_variant"
            | "variant_bytes_to_variant"
    )
}

/// `variant_extract(child, key)`, which is what `child.key` and `child[key]` bind to.
pub(super) fn bind_extract(child: BoundExpr, key: BoundExpr) -> Result<BoundExpr> {
    let BoundExprKind::Constant(constant) = &key.kind else {
        return Err(binder_error(
            "'variant_extract' expects the second argument to be a constant",
        ));
    };
    if constant.logical_type().is_integral() && constant.as_i128() == Some(0) {
        return Err(binder_error(
            "Extracting index 0 from VARIANT(ARRAY) is invalid, indexes are 1-based",
        ));
    }
    Ok(bound_function(
        "variant_extract",
        vec![child, key],
        variant_type(),
        variant_extract,
    ))
}

fn as_variant(child: BoundExpr) -> BoundExpr {
    child.cast_to(&variant_type(), false)
}

/// Binds a function of [`is_variant_function`].
pub fn bind_variant_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let lower = name.to_ascii_lowercase();
    if lower == "variant_to_parquet_variant" || lower == "variant_bytes_to_variant" {
        return bind_parquet_variant_function(&lower, children);
    }
    let mut children = children.into_iter();
    let (Some(child), key, None) = (children.next(), children.next(), children.next()) else {
        return Err(binder_error(format!(
            "{lower} expects either one VARIANT column argument, or a VARIANT column and a path"
        )));
    };
    let child = as_variant(child);
    match (lower.as_str(), key) {
        ("variant_typeof", None) => Ok(bound_function(
            &lower,
            vec![child],
            LogicalType::VARCHAR,
            variant_typeof,
        )),
        ("variant_array_length", None) => Ok(bound_function(
            &lower,
            vec![child],
            LogicalType::BIGINT,
            variant_array_length,
        )),
        ("variant_keys", None) => Ok(bound_function(
            &lower,
            vec![child],
            LogicalType::list(LogicalType::VARCHAR),
            variant_keys,
        )),
        ("variant_keys", Some(key)) => Ok(bound_function(
            &lower,
            vec![child, key],
            LogicalType::list(LogicalType::VARCHAR),
            variant_keys,
        )),
        ("variant_extract", Some(key)) => bind_extract(child, key),
        _ => Err(binder_error(format!(
            "{lower} expects either one VARIANT column argument, or a VARIANT column and a path"
        ))),
    }
}

/// `variant_to_parquet_variant` and `variant_bytes_to_variant` of the Parquet extension.
fn bind_parquet_variant_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let mut children = children.into_iter();
    let (Some(child), None) = (children.next(), children.next()) else {
        return Err(binder_error(format!(
            "No function matches the given name and argument types for '{name}'"
        )));
    };
    if name == "variant_to_parquet_variant" {
        let ty = crate::execution::parquet_variant::parquet_variant_type(None)?.with_alias("PARQUET_VARIANT");
        return Ok(bound_function(
            name,
            vec![as_variant(child)],
            ty,
            variant_to_parquet_variant,
        ));
    }
    Ok(bound_function(
        name,
        vec![child.cast_to(&LogicalType::BLOB, false)],
        variant_type(),
        variant_bytes_to_variant,
    ))
}

fn variant_to_parquet_variant(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        crate::execution::parquet_variant::to_parquet_variant(&row[0], None, &ty)
    })
}

fn variant_bytes_to_variant(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let ValueData::Bytes(bytes) = row[0].data() else {
            return Ok(Value::null(variant_type()));
        };
        if row[0].is_null() {
            return Ok(Value::null(variant_type()));
        }
        match crate::execution::parquet_variant::from_bytes(bytes)? {
            VariantNode::Null => Ok(Value::null(variant_type())),
            node => Ok(variant_value(&node)),
        }
    })
}
