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
/// The child at `key`, moved out of `node` (a clone of a deep child would recurse).
fn extract_child(node: VariantNode, key: &Value) -> Option<VariantNode> {
    let mut node = node;
    match (&mut node, key.data()) {
        (VariantNode::Object(children), ValueData::Bytes(key)) => {
            let key = String::from_utf8_lossy(key);
            let at = children.iter().position(|(k, _)| k.eq_ignore_ascii_case(&key))?;
            Some(children.swap_remove(at).1)
        }
        (VariantNode::Array(items), _) => {
            let index = key.as_i128().filter(|i| *i >= 1)? as usize;
            (index <= items.len()).then(|| items.swap_remove(index - 1))
        }
        _ => None,
    }
}

fn variant_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(variant_type()));
        }
        Ok(match extract_child(decode(&row[0])?, &row[1]) {
            // a NULL inside of a VARIANT is NULL once it is taken out
            Some(VariantNode::Null) | None => Value::null(variant_type()),
            Some(node) => variant_value(&node),
        })
    })
}

/// `WriteKeysResult`: the keys of an object in lexicographic order, none of anything else.
fn variant_keys(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_paths(args, result, &LogicalType::list(LogicalType::VARCHAR), &|node| {
        let ty = LogicalType::list(LogicalType::VARCHAR);
        Ok(match &node {
            None => Value::null(ty),
            Some(VariantNode::Object(children)) => {
                let mut keys: Vec<&String> = children.iter().map(|(k, _)| k).collect();
                keys.sort();
                Value::list(LogicalType::VARCHAR, keys.into_iter().map(Value::varchar).collect())?
            }
            Some(_) => Value::list(LogicalType::VARCHAR, Vec::new())?,
        })
    })
}

/// `WriteTypeResult`: the `VariantLogicalType` of the value.
fn variant_type_name(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_paths(args, result, &LogicalType::VARCHAR, &|node| {
        Ok(match &node {
            None => Value::null(LogicalType::VARCHAR),
            Some(VariantNode::Scalar(value)) if value.logical_type().id() == LogicalTypeId::Decimal => {
                Value::varchar("DECIMAL")
            }
            Some(node) => Value::varchar(node.type_name()),
        })
    })
}

fn variant_array_length(args: &DataChunk, result: &mut Vector) -> Result<()> {
    // `WriteArrayLengthsResult`: 0 for what is no array, NULL for what is not there
    over_paths(args, result, &LogicalType::UBIGINT, &|node| {
        Ok(match &node {
            Some(VariantNode::Array(items)) => Value::ubigint(items.len() as u64),
            Some(_) => Value::ubigint(0),
            None => Value::null(LogicalType::UBIGINT),
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
            | "variant_type"
            | "variant_contains"
            | "variant_normalize"
            | "variant_array_length"
            | "variant_exists"
            | "variant_extract_string"
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
    if matches!(
        lower.as_str(),
        "variant_exists" | "variant_extract_string" | "variant_array_length" | "variant_keys" | "variant_type"
    ) {
        return bind_path_function(&lower, children);
    }
    if lower == "variant_contains" {
        let [haystack, needle] = <[BoundExpr; 2]>::try_from(children).map_err(|children| {
            binder_error(format!(
                "No function matches the given name and argument types 'variant_contains({})'. You might need to \
                 add explicit type casts.",
                children
                    .iter()
                    .map(|c| super::binder::expression_return_type(c).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        return Ok(bound_function(
            &lower,
            vec![as_variant(haystack), as_variant(needle)],
            LogicalType::BOOLEAN,
            variant_contains,
        ));
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
        ("variant_extract", Some(key)) => bind_extract(child, key),
        ("variant_normalize", None) => Ok(bound_function(&lower, vec![child], variant_type(), variant_normalize)),
        _ => Err(binder_error(format!(
            "{lower} expects either one VARIANT column argument, or a VARIANT column and a path"
        ))),
    }
}

// Ported from duckdb/src/function/scalar/variant/{variant_path_function,variant_bind_utils,variant_exists,
// variant_extract_string,variant_array_length}.cpp @ rust-port-baseline

/// `VariantPathFunction::CreateFunctionSet` + `VariantBindUtils::VariantPathBind`: a function of a VARIANT at a path
/// (one key of an object, or the value itself for an empty one), or at each of a list of paths.
fn bind_path_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let (callback, element): (super::ScalarCallback, LogicalType) = match name {
        "variant_exists" => (variant_exists, LogicalType::BOOLEAN),
        "variant_extract_string" => (variant_extract_string, LogicalType::VARCHAR),
        "variant_keys" => (variant_keys, LogicalType::list(LogicalType::VARCHAR)),
        "variant_type" => (variant_type_name, LogicalType::VARCHAR),
        _ => (variant_array_length, LogicalType::UBIGINT),
    };
    let mut children = children;
    // `path_optional`: variant_exists and variant_extract_string need a path
    if children.len() == 1 && matches!(name, "variant_exists" | "variant_extract_string") {
        return Err(binder_error(format!(
            "No function matches the given name and argument types '{name}({})'. You might need to add explicit \
             type casts.",
            super::binder::expression_return_type(&children[0])
        )));
    }
    if children.len() != 1 && children.len() != 2 {
        return Err(binder_error(format!(
            "{name} expects either one VARIANT column argument, or two VARIANT column and VARCHAR path arguments"
        )));
    }
    children[0] = as_variant(children[0].clone());
    if children.len() == 1 {
        return Ok(bound_function(name, children, element, callback));
    }
    let path = &children[1];
    let path_type = path.return_type.clone();
    // a NULL path makes the function NULL, as any NULL argument does
    if path_type.id() == LogicalTypeId::SqlNull {
        return Ok(BoundExpr::constant(Value::null(element)));
    }
    let list = match path_type.id() {
        LogicalTypeId::Varchar => false,
        LogicalTypeId::List
            if matches!(
                path_type.child_type().map(LogicalType::id),
                Some(LogicalTypeId::Varchar | LogicalTypeId::SqlNull)
            ) =>
        {
            true
        }
        // no overload takes it (`FunctionBinder::BindFunction`)
        _ => {
            return Err(binder_error(format!(
                "No function matches the given name and argument types '{name}({}, {})'. You might need to add \
                 explicit type casts.",
                children[0].return_type,
                super::binder::expression_return_type(&children[1])
            )));
        }
    };
    if !path.is_foldable() {
        return Err(binder_error(format!("{name} expects the path to be a constant")));
    }
    let value = crate::execution::expression_executor::evaluate_scalar(path)?;
    if value.is_null() {
        return Err(binder_error(format!("{name} does not accept NULL paths")));
    }
    if let ValueData::List(items) = value.data()
        && items.iter().any(Value::is_null)
    {
        return Err(binder_error(format!("'{name}' does not accept NULL paths")));
    }
    children[1] = BoundExpr::constant(value);
    let ty = if list { LogicalType::list(element) } else { element };
    Ok(bound_function(name, children, ty, callback))
}

/// `VariantPathFunction::ResolvePath`: the value at a key of an object, if it is there.
fn resolve_path(node: VariantNode, key: &str) -> Option<VariantNode> {
    if key.is_empty() {
        return Some(node);
    }
    let mut node = node;
    match &mut node {
        VariantNode::Object(children) => std::mem::take(children)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, child)| child),
        _ => None,
    }
}

/// Each row's value at the path (or the paths) of a path function, as `write` makes it.
fn over_paths(
    args: &DataChunk,
    result: &mut Vector,
    element: &LogicalType,
    write: &dyn Fn(Option<VariantNode>) -> Result<Value>,
) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let node = decode(&row[0])?;
        match row.get(1).map(Value::data) {
            None => write(Some(node)),
            Some(ValueData::List(paths)) => {
                let items = paths
                    .iter()
                    .map(|path| write(resolve_path(node.clone(), &path.try_to_string()?)))
                    .collect::<Result<Vec<_>>>()?;
                Value::list(element.clone(), items)
            }
            Some(_) => write(resolve_path(node, &row[1].try_to_string()?)),
        }
    })
}

fn variant_exists(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_paths(args, result, &LogicalType::BOOLEAN, &|node| {
        Ok(Value::boolean(node.is_some()))
    })
}

/// `VariantStringWriter`: a VARCHAR as it is, anything else as its JSON, and a JSON string without its quotes.
fn variant_extract_string(args: &DataChunk, result: &mut Vector) -> Result<()> {
    over_paths(args, result, &LogicalType::VARCHAR, &|node| {
        let node = match node {
            None | Some(VariantNode::Null) => return Ok(Value::null(LogicalType::VARCHAR)),
            Some(node) => node,
        };
        if let VariantNode::Scalar(value) = &node
            && value.logical_type().id() == LogicalTypeId::Varchar
        {
            return Ok(Value::varchar(value.try_to_string()?));
        }
        let json = super::json::value_to_json(&variant_value(&node))?;
        if let duckrs_common::json::Json::Str(text) = &json {
            return Ok(Value::varchar(text.clone()));
        }
        let mut out = String::new();
        json.write(&mut out);
        Ok(Value::varchar(out))
    })
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

// Ported from duckdb/src/function/scalar/variant/{variant_contains,variant_comparator}.cpp

/// `GetVariantComparisonType` with the value it compares by: numbers by their decimal digits,
/// reals as doubles with every NaN alike, times and timestamps as nanoseconds.
#[derive(PartialEq)]
enum Comparable {
    Null,
    Boolean(bool),
    Number(String),
    Real(u64),
    Uuid(i128),
    Timestamp(i128),
    TimestampTz(i128),
    Time(i128),
    TimeTz(i128),
    Interval(i128),
    Varchar(Vec<u8>),
    Blob(Vec<u8>),
    Bitstring(Vec<u8>),
    Geometry(Vec<u8>),
    Array,
    Object,
}

/// A number's digits without trailing zeros after the point (`VariantGetNumberKey`).
fn number_text(unscaled: String, scale: usize) -> String {
    let (negative, digits) = match unscaled.strip_prefix('-') {
        Some(digits) => (true, digits.to_string()),
        None => (false, unscaled),
    };
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    let whole = whole.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let text = if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    };
    if negative && text != "0" { format!("-{text}") } else { text }
}

fn comparable(node: &VariantNode) -> Result<Comparable> {
    use LogicalTypeId as Id;
    let value = match node {
        VariantNode::Null => return Ok(Comparable::Null),
        VariantNode::Array(_) => return Ok(Comparable::Array),
        VariantNode::Object(_) => return Ok(Comparable::Object),
        VariantNode::Scalar(value) => value,
    };
    let int = || value.as_i128().unwrap_or(0);
    let bytes = || match value.data() {
        ValueData::Bytes(bytes) => bytes.clone(),
        _ => Vec::new(),
    };
    Ok(match value.logical_type().id() {
        Id::Boolean => Comparable::Boolean(*value == Value::boolean(true)),
        Id::TinyInt | Id::SmallInt | Id::Integer | Id::BigInt | Id::HugeInt | Id::UTinyInt | Id::USmallInt
        | Id::UInteger | Id::UBigInt | Id::UHugeInt | Id::Bignum => {
            Comparable::Number(number_text(value.try_to_string()?, 0))
        }
        Id::Decimal => {
            let (_, scale) = value.logical_type().decimal_properties().unwrap_or((18, 0));
            Comparable::Number(number_text(int().to_string(), usize::from(scale)))
        }
        Id::Float | Id::Double => {
            let real = match value.data() {
                ValueData::Float(f) => f64::from(*f),
                ValueData::Double(d) => *d,
                _ => 0.0,
            };
            // `Radix::EncodeDouble`: every NaN alike, and -0.0 is 0.0
            let real = if real.is_nan() { f64::NAN } else if real == 0.0 { 0.0 } else { real };
            Comparable::Real(real.to_bits())
        }
        Id::Uuid => Comparable::Uuid(int()),
        Id::Date => Comparable::Timestamp(int() * 86_400_000_000_000),
        Id::TimestampSec => Comparable::Timestamp(int() * 1_000_000_000),
        Id::TimestampMs => Comparable::Timestamp(int() * 1_000_000),
        Id::Timestamp => Comparable::Timestamp(int() * 1000),
        Id::TimestampNs => Comparable::Timestamp(int()),
        Id::TimestampTz => Comparable::TimestampTz(int() * 1000),
        Id::TimestampTzNs => Comparable::TimestampTz(int()),
        Id::Time => Comparable::Time(int() * 1000),
        Id::TimeNs => Comparable::Time(int()),
        Id::TimeTz => Comparable::TimeTz(int()),
        Id::Interval => match value.data() {
            ValueData::Interval(i) => Comparable::Interval(
                (i128::from(i.months) * 30 + i128::from(i.days)) * 86_400_000_000 + i128::from(i.micros),
            ),
            _ => Comparable::Interval(0),
        },
        Id::Blob => Comparable::Blob(bytes()),
        Id::Bit => Comparable::Bitstring(bytes()),
        Id::Geometry => Comparable::Geometry(bytes()),
        _ => Comparable::Varchar(bytes()),
    })
}

/// `IsEqual`: a needle's array items and object entries each have to be somewhere in the haystack's.
fn contains_equal(haystack: &VariantNode, needle: &VariantNode) -> Result<bool> {
    let category = comparable(haystack)?;
    let needle_category = comparable(needle)?;
    match (haystack, needle) {
        (VariantNode::Array(hay), VariantNode::Array(items)) => {
            for item in items {
                let mut found = false;
                for candidate in hay {
                    if contains_equal(candidate, item)? {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (VariantNode::Object(hay), VariantNode::Object(entries)) => {
            for (key, item) in entries {
                let mut found = false;
                for (candidate_key, candidate) in hay {
                    if candidate_key == key && contains_equal(candidate, item)? {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(category == needle_category),
    }
}

/// `RecursiveHaystackWalk`
fn haystack_walk(haystack: &VariantNode, needle: &VariantNode) -> Result<bool> {
    if contains_equal(haystack, needle)? {
        return Ok(true);
    }
    match haystack {
        VariantNode::Array(items) => {
            for item in items {
                if haystack_walk(item, needle)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        VariantNode::Object(children) => {
            for (_, child) in children {
                if haystack_walk(child, needle)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn variant_contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() || row[1].is_null() {
            return Ok(Value::null(LogicalType::BOOLEAN));
        }
        Ok(Value::boolean(haystack_walk(&decode(&row[0])?, &decode(&row[1])?)?))
    })
}

/// `VariantNormalizer::Normalize`: the value again, with its objects' keys in order (which a decoded
/// object already has), after `VariantVisitor::CheckDepth`.
fn variant_normalize(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(variant_type()));
        }
        let node = decode(&row[0])?;
        let mut stack = vec![(&node, 0usize)];
        while let Some((node, depth)) = stack.pop() {
            if depth >= 128 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "VARIANT exceeds maximum recursion depth of 128",
                ));
            }
            match node {
                VariantNode::Array(items) => stack.extend(items.iter().map(|item| (item, depth + 1))),
                VariantNode::Object(children) => stack.extend(children.iter().map(|(_, child)| (child, depth + 1))),
                _ => {}
            }
        }
        Ok(variant_value(&node))
    })
}
