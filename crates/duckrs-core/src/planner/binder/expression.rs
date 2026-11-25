// Ported from duckdb/src/planner/binder/expression/bind_{columnref,comparison,conjunction,
// constant,cast,function,operator}_expression.cpp, duckdb/src/parser/literal.cpp
// (NumberToValue), duckdb/src/function/scalar/comparison/comparison.cpp (ComparisonBind) and
// duckdb/src/catalog/default/default_types.cpp @ rust-port-baseline
//
//! Binding parsed expressions into [`BoundExpr`]s.

use super::{Binder, ColumnMode, CorrelatedColumn, binder_error};
use crate::catalog::{DEFAULT_CATALOG, SequenceCatalogEntry};
use crate::execution::expression_executor::{evaluate_scalar, try_evaluate_scalar};
use crate::function::binder::{bind_aggregate_function, bind_scalar_function, expression_return_type};
use crate::function::combine_types::{force_max_logical_type, normalize_type, try_max_logical_type};
use crate::function::generic::{bind_generic_function, bind_unnest, is_generic_function, is_unnest};
use crate::function::holistic::negate_fractions;
use crate::function::math::bind_decimal_round;
use crate::function::nested::{bind_nested_function, is_nested_function};
use crate::function::variant::{bind_variant_function, is_variant_function};
use crate::function::{BuiltinFunctions, typeof_constant};
use crate::optimizer::optimize;
use crate::planner::column_binding_resolver::resolve_column_bindings;
use crate::planner::expression::{
    BoundExpr, BoundExprKind, BoundOperator, BoundSubquery, ColumnBinding, SequenceOp, SubqueryKind,
};
use crate::planner::subquery::flatten_subqueries;
use duckrs_common::string_util::similarity_rating;
use duckrs_common::types::{LogicalType, LogicalTypeId, bignum, bit, decimal};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast;
use duckrs_parser::ast::sql_quoted_identifier;
use duckrs_parser::ast::{
    CastType, ComparisonOp, Expr, ExprKind, FunctionExpr, Literal, LiteralKind, OperatorType, SubqueryType, TypeExpr,
};
use duckrs_parser::transformer::visit_expr_mut;
use parking_lot::Mutex;
use std::sync::Arc;

/// `NumberToValue`: an integer literal is INTEGER, BIGINT, HUGEINT or UHUGEINT if it fits, a
/// number with a fraction is a DECIMAL as wide as it is written, anything else a DOUBLE.
fn number_to_value(text: &str) -> Result<Value> {
    let bytes = text.as_bytes();
    let mut try_integer = true;
    let mut try_decimal = true;
    let mut decimal_position = None;
    let mut underscores = 0;
    let mut integer_underscores = 0;
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'.' {
            try_integer = false;
            decimal_position = Some(i);
        }
        if c == b'e' || c == b'E' {
            try_integer = false;
            try_decimal = false;
        }
        if c == b'_' {
            underscores += 1;
            if decimal_position.is_none() {
                integer_underscores += 1;
            }
        }
    }
    let digits: String = text.chars().filter(|&c| c != '_').collect();
    if try_integer {
        if let Ok(v) = digits.parse::<i32>() {
            // the magnitude of INT32_MIN does not fit an INTEGER, so -2147483648 is typed as BIGINT
            if v != i32::MIN {
                return Ok(Value::integer(v));
            }
        }
        if let Ok(v) = digits.parse::<i64>() {
            return Ok(Value::bigint(v));
        }
        if let Ok(v) = digits.parse::<i128>() {
            return Ok(Value::hugeint(v));
        }
        if let Ok(v) = digits.parse::<u128>() {
            return Ok(Value::uhugeint(v));
        }
        let digits = digits.as_bytes();
        let bignum = bignum::from_str(digits).expect("the digits of an integer literal");
        return Value::new(LogicalType::new(LogicalTypeId::Bignum), ValueData::Bytes(bignum));
    }
    let decimal_offset = if bytes[0] == b'-' { 3 } else { 2 };
    if let Some(position) = decimal_position
        && try_decimal
        && bytes.len() - underscores < decimal::MAX_WIDTH as usize + decimal_offset
    {
        let mut width = bytes.len() - 1 - underscores;
        if position > width + integer_underscores {
            return Err(Error::new(
                ErrorKind::Parser,
                format!("Could not convert string \"{text}\" to a decimal value"),
            ));
        }
        let scale = width - position + integer_underscores;
        if bytes[0] == b'-' {
            width -= 1;
        }
        if width <= decimal::MAX_WIDTH as usize {
            let unscaled_digits: String = digits.chars().filter(|&c| c != '.').collect();
            let unscaled = if unscaled_digits.trim_start_matches('-').is_empty() {
                0
            } else {
                unscaled_digits.parse::<i128>().map_err(|_| {
                    Error::new(
                        ErrorKind::Conversion,
                        format!("Could not convert string \"{text}\" to DECIMAL"),
                    )
                })?
            };
            return Value::decimal(unscaled, width.max(1) as u8, scale as u8);
        }
    }
    // there is an exponent, or the value is too wide for a decimal
    digits.parse::<f64>().map(Value::double).map_err(|_| {
        Error::new(
            ErrorKind::Conversion,
            format!("Could not convert string '{text}' to DOUBLE"),
        )
    })
}

/// `Literal::ToValue`.
pub(crate) fn literal_to_value(literal: &Literal) -> Result<Value> {
    match literal.kind {
        LiteralKind::Null => Ok(Value::sql_null()),
        LiteralKind::Boolean => Ok(Value::boolean(literal.text == "true")),
        LiteralKind::Integer | LiteralKind::Numeric => number_to_value(&literal.text),
        LiteralKind::String => Ok(Value::varchar(literal.text.clone())),
        LiteralKind::Hex => {
            let bytes = (0..literal.text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&literal.text[i..i + 2], 16).expect("validated by the parser"))
                .collect::<Vec<u8>>();
            Ok(Value::blob(bytes))
        }
        LiteralKind::Bit => {
            let bytes =
                bit::string_to_bit(literal.text.as_bytes()).map_err(|e| Error::new(ErrorKind::Conversion, e))?;
            Value::new(LogicalType::new(LogicalTypeId::Bit), ValueData::Bytes(bytes))
        }
    }
}

/// `ArrayType::MAX_ARRAY_SIZE` and `UnionType::MAX_UNION_MEMBERS`.
const MAX_ARRAY_SIZE: i64 = 100_000;
const MAX_UNION_MEMBERS: usize = 255;

/// The names a misspelled type is compared with.
const BUILTIN_TYPE_NAMES: &[&str] = &[
    "bigint",
    "bit",
    "blob",
    "boolean",
    "date",
    "decimal",
    "double",
    "float",
    "hugeint",
    "integer",
    "interval",
    "real",
    "smallint",
    "text",
    "time",
    "timestamp",
    "tinyint",
    "ubigint",
    "uhugeint",
    "uinteger",
    "usmallint",
    "utinyint",
    "uuid",
    "varchar",
];

/// The built-in type names and their aliases (`BUILTIN_TYPES`), for types without parameters.
fn builtin_type(name: &str) -> Option<LogicalTypeId> {
    use LogicalTypeId::*;
    Some(match name {
        "decimal" | "dec" | "numeric" => Decimal,
        "time" => Time,
        "time_ns" => TimeNs,
        "date" => Date,
        "timestamp" | "datetime" | "timestamp_us" => Timestamp,
        "timestamp_ms" => TimestampMs,
        "timestamp_ns" => TimestampNs,
        "timestamp_s" => TimestampSec,
        "timestamptz" | "timestamp with time zone" => TimestampTz,
        "timestamptz_ns" => TimestampTzNs,
        "timetz" | "time with time zone" => TimeTz,
        "interval" => Interval,
        "varchar" | "bpchar" | "string" | "char" | "nvarchar" | "text" => Varchar,
        "blob" | "bytea" | "varbinary" | "binary" => Blob,
        "hugeint" | "int128" => HugeInt,
        "uhugeint" | "uint128" => UHugeInt,
        "bigint" | "oid" | "long" | "int8" | "int64" => BigInt,
        "ubigint" | "uint64" => UBigInt,
        "integer" | "int" | "int4" | "signed" | "integral" | "int32" => Integer,
        "uinteger" | "uint32" => UInteger,
        "smallint" | "int2" | "short" | "int16" => SmallInt,
        "usmallint" | "uint16" => USmallInt,
        "tinyint" | "int1" => TinyInt,
        "utinyint" | "uint8" => UTinyInt,
        "struct" | "row" => Struct,
        "tuple" => Tuple,
        "list" => List,
        "array" => Array,
        "map" => Map,
        "union" => Union,
        "bit" | "bitstring" => Bit,
        "variant" => Variant,
        "bignum" | "varint" => Bignum,
        "boolean" | "bool" | "logical" => Boolean,
        "uuid" | "guid" => Uuid,
        "enum" => Enum,
        "null" => SqlNull,
        "float" | "real" | "float4" => Float,
        "double" | "float8" => Double,
        "geometry" => Geometry,
        "type" => Type,
        _ => return None,
    })
}

/// A non-negative integer type parameter.
fn type_parameter(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Constant(literal) if literal.kind == LiteralKind::Integer => literal.text.parse().ok(),
        _ => None,
    }
}

/// Whether `name` is a built-in type, which `CREATE TYPE` cannot shadow.
pub fn is_builtin_type(name: &str) -> bool {
    builtin_type(&name.to_lowercase()).is_some()
}

/// `LogicalType::ENUM`: values must be unique.
pub fn enum_type_from(values: Vec<String>) -> Result<LogicalType> {
    for (i, value) in values.iter().enumerate() {
        if values[..i].contains(value) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Attempted to create ENUM type with duplicate value {value}"),
            ));
        }
    }
    Ok(LogicalType::enum_type(values))
}

/// The type of a type parameter, for `TypeParametersToString`.
fn type_parameter_type(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Constant(literal) => match literal.kind {
            LiteralKind::Integer => "INTEGER".to_string(),
            LiteralKind::String => "VARCHAR".to_string(),
            LiteralKind::Null => "NULL".to_string(),
            LiteralKind::Boolean => "BOOLEAN".to_string(),
            // the DECIMAL a number literal reads as: all its digits, and those after the point
            _ => {
                let text = literal.text.trim_start_matches('-');
                let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
                let whole = whole.trim_start_matches('0');
                let width = (whole.len() + fraction.len()).max(1);
                format!("DECIMAL({width},{})", fraction.len())
            }
        },
        // a negative number is a parenthesized expression here
        _ => "INTEGER".to_string(),
    }
}

/// `CastArgument`: a NULL type parameter is refused by its name.
fn null_type_parameter(type_name: &str, names: &[&str], children: &[Expr]) -> Result<()> {
    for (child, name) in children.iter().zip(names) {
        if matches!(&child.kind, ExprKind::Constant(literal) if literal.kind == LiteralKind::Null) {
            return Err(binder_error(format!(
                "Type parameter \"{name}\" for type \"{type_name}\" cannot be NULL"
            )));
        }
    }
    Ok(())
}

/// `ColumnQualifier::SQLValueFunctionName`: what these words call when no column has their name.
pub(super) fn sql_value_function(name: &str) -> Option<&'static str> {
    match name.to_lowercase().as_str() {
        "current_catalog" => Some("current_catalog"),
        "current_date" => Some("current_date"),
        "current_schema" => Some("current_schema"),
        "current_role" => Some("current_role"),
        "current_time" => Some("get_current_time"),
        "current_timestamp" => Some("get_current_timestamp"),
        "current_user" => Some("current_user"),
        "localtime" => Some("current_localtime"),
        "localtimestamp" => Some("current_localtimestamp"),
        "session_user" => Some("session_user"),
        "user" => Some("user"),
        _ => None,
    }
}

/// Every expression of a query, its set operations and WITH clause included, and the same in the queries of its
/// subqueries.
fn visit_query_and_subqueries(node: &mut ast::QueryNode, f: &mut dyn FnMut(&mut Expr)) {
    duckrs_parser::transformer::visit_all_query_node_exprs_mut(node, &mut |e| {
        f(e);
        if let ExprKind::Subquery { subquery, .. } = &mut e.kind {
            visit_query_and_subqueries(&mut subquery.node, f);
        }
    });
}

/// What an aggregate of an outer query's columns is, until the subquery around it moves it out.
pub(super) const OUTER_AGGREGATE: &str = "aggregates of the columns of an outer query";

impl Binder<'_> {
    /// `Binder::BindLogicalType` for the types slice 1 executes: the scalar types without
    /// parameters, `VARCHAR(n)` and `DECIMAL(width, scale)`.
    /// A type of a database file that names a type of the catalog: its SQL is what would name it
    /// in a query.
    /// `BindMakeTypeFunctionExpression`: the arguments, constants, spelled as the type they make, then bound.
    fn bind_make_type(&mut self, function: &ast::FunctionExpr) -> Result<BoundExpr> {
        let mut arguments = Vec::with_capacity(function.args.len());
        for argument in &function.args {
            let bound = self.bind_expression(&argument.expr)?;
            if !bound.is_foldable() {
                return Err(binder_error(
                    "make_type function arguments must be constant expressions",
                ));
            }
            let value = crate::execution::expression_executor::evaluate_scalar(&bound)?;
            arguments.push((argument.name.clone(), value));
        }
        let Some((_, name)) = arguments.first() else {
            return Err(binder_error("make_type function requires at least one argument"));
        };
        if name.logical_type().id() != LogicalTypeId::Varchar || name.is_null() {
            return Err(binder_error(
                "make_type function first argument must be the type name as VARCHAR",
            ));
        }
        let name = name.try_to_string()?;
        // `QualifiedName::TryParse` of the name
        parse_qualified_name(&name)?;
        let upper = name.to_uppercase();
        let parameter = |(alias, value): &(String, Value)| -> Result<String> {
            let text = match value.logical_type().id() {
                LogicalTypeId::Type => value.try_to_string()?,
                LogicalTypeId::Varchar => format!("'{}'", value.try_to_string()?.replace('\'', "''")),
                _ => value.try_to_string()?,
            };
            Ok(if alias.is_empty() {
                text
            } else {
                format!("{} {text}", quote_identifier(alias))
            })
        };
        let parameters = arguments[1..].iter().map(parameter).collect::<Result<Vec<_>>>()?;
        if upper == "STRUCT" && arguments[1..].iter().any(|(alias, _)| alias.is_empty()) {
            let types: Vec<String> = arguments[1..]
                .iter()
                .map(|(_, v)| v.logical_type().to_string())
                .collect();
            return Err(binder_error(format!(
                "Type \"STRUCT\" does not accept type parameters ({})",
                types.join(", ")
            )));
        }
        let text = match (upper.as_str(), parameters.as_slice()) {
            (_, []) => quote_identifier(&name),
            ("LIST", [child]) => format!("{child}[]"),
            _ => format!("{}({})", quote_identifier(&name), parameters.join(", ")),
        };
        let ty = self.bind_named_type(&LogicalType::unbound(text))?;
        Ok(BoundExpr::constant(type_value(&ty)))
    }

    fn bind_named_type(&self, ty: &LogicalType) -> Result<LogicalType> {
        let statements = duckrs_parser::parse_ast(&format!("SELECT CAST(NULL AS {ty})"))?;
        if let [ast::Statement::Select(select)] = statements.as_slice()
            && let ast::QueryNodeKind::Select(node) = &select.node.kind
            && let [
                Expr {
                    kind: ExprKind::Cast { target, .. },
                    ..
                },
            ] = node.select_list.as_slice()
        {
            return self.bind_type(target);
        }
        Err(crate::unported(format!("the type {ty}")))
    }

    pub fn bind_type(&self, target: &CastType) -> Result<LogicalType> {
        let (original, children): (&str, &[Expr]) =
            match target {
                CastType::Resolved(ty) if ty.contains_unbound() => return self.bind_named_type(ty),
                CastType::Resolved(ty) => return Ok(ty.clone()),
                CastType::Builtin(name) => (&**name, &[]),
                CastType::Parsed(expr) => match &expr.kind {
                    ExprKind::Type(TypeExpr { name, children }) => {
                        if !name.qualification.is_empty() {
                            let schema = self.schema_of(name)?;
                            // `Catalog::LookupEntry`: a missing schema is a missing type, except that a nested
                            // schema path is named
                            let found = self.catalog.get_type(schema.as_deref(), None, &name.name).or_else(|error| {
                            if error.kind != ErrorKind::Catalog || !error.message.starts_with("Schema with name") {
                                return Err(error);
                            }
                            if name.qualification.len() >= 2 {
                                let schema = name.qualification.join(".");
                                return Err(Error::new(
                                    ErrorKind::Catalog,
                                    format!(
                                        "Type with name \"{schema}.{}\" does not exist because schema \"{schema}\" \
                                         does not exist.",
                                        name.name
                                    ),
                                ));
                            }
                            Ok(None)
                        })?;
                            if let Some(ty) = found {
                                return Ok(ty);
                            }
                            // `catalog.type`: a name that is no schema can be a database
                            if let [catalog] = name.qualification.as_slice() {
                                // without a session (a stored definition) the database is this one
                                let attached = if catalog.eq_ignore_ascii_case(&self.catalog.name)
                                    || self
                                        .session
                                        .as_ref()
                                        .is_none_or(|s| s.database.eq_ignore_ascii_case(catalog))
                                {
                                    self.catalog.get_type(Some("main"), None, &name.name)?
                                } else {
                                    self.session
                                        .as_ref()
                                        .and_then(|session| (session.attached_catalog)(catalog))
                                        .map(|attached| attached.get_type(Some("main"), None, &name.name))
                                        .transpose()?
                                        .flatten()
                                };
                                if let Some(ty) = attached {
                                    return Ok(ty);
                                }
                            }
                            // the built-in types are found in `main` too
                            let in_main = name
                                .qualification
                                .iter()
                                .rev()
                                .find(|q| !q.is_empty())
                                .is_some_and(|q| q.eq_ignore_ascii_case("main"));
                            if !in_main || builtin_type(&name.name.to_lowercase()).is_none() {
                                return Err(Error::new(
                                    ErrorKind::Catalog,
                                    format!("Type with name {} does not exist!", name.name),
                                ));
                            }
                        }
                        (name.name.as_str(), children.as_slice())
                    }
                    _ => return Err(crate::unported(format!("type expression {expr}"))),
                },
            };
        let name = original.to_lowercase();
        if let Some(ty) = self.catalog.get_type(None, self.search_schema.as_deref(), &name)? {
            if !children.is_empty() {
                return Err(binder_error(format!(
                    "Type \"{original}\" does not take any type parameters"
                )));
            }
            return Ok(ty);
        }
        if name == "json" {
            return Ok(crate::function::json::json_type());
        }
        let Some(id) = builtin_type(&name) else {
            let closest = BUILTIN_TYPE_NAMES
                .iter()
                .map(|candidate| (similarity_rating(candidate, &name), *candidate))
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .filter(|(score, _)| *score > 0.7);
            // `CreateMissingEntryException`: a type of that name in a schema off the search path
            let mut elsewhere: Vec<String> = self
                .catalog
                .schemas()
                .iter()
                .filter(|schema| schema.get_type(&name).is_some())
                .map(|schema| format!("{}.{original}", schema.name))
                .collect();
            elsewhere.sort();
            let hint = if elsewhere.is_empty() {
                closest.map_or_else(String::new, |(_, type_name)| format!("\nDid you mean \"{type_name}\"?"))
            } else {
                format!("\nDid you mean \"{}\"?", elsewhere.join(" or "))
            };
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Type with name {original} does not exist!{hint}"),
            ));
        };
        match id {
            LogicalTypeId::Decimal => {
                let rejected = || {
                    let types: Vec<String> = children.iter().map(type_parameter_type).collect();
                    binder_error(format!(
                        "Type \"DECIMAL\" does not accept type parameters ({})\nCandidates:\n    DECIMAL\n    \
                         DECIMAL(width UTINYINT, scale UTINYINT := 0)",
                        types.join(", ")
                    ))
                };
                if children.len() <= 2 {
                    null_type_parameter("DECIMAL", &["width", "scale"], children)?;
                }
                let (width, scale) = match children {
                    [] => (18, 3),
                    [w] => (type_parameter(w).ok_or_else(rejected)?, 0),
                    [w, s] => (
                        type_parameter(w).ok_or_else(rejected)?,
                        type_parameter(s).ok_or_else(rejected)?,
                    ),
                    _ => return Err(rejected()),
                };
                // the parameters are UTINYINT
                if !(0..=255).contains(&width) || !(0..=255).contains(&scale) {
                    return Err(rejected());
                }
                if !(1..=decimal::MAX_WIDTH as i64).contains(&width) {
                    return Err(binder_error(format!(
                        "DECIMAL type width must be between 1 and {}",
                        decimal::MAX_WIDTH
                    )));
                }
                if !(0..=width).contains(&scale) {
                    return Err(binder_error("DECIMAL type scale cannot be greater than width"));
                }
                Ok(LogicalType::decimal(width as u8, scale as u8))
            }
            // a string parameter is a collation (`VARCHAR COLLATE x` reads as `VARCHAR('x')`)
            LogicalTypeId::Varchar
                if let [child] = children
                    && let ExprKind::Constant(literal) = &child.kind
                    && literal.kind == LiteralKind::String =>
            {
                crate::planner::collation::test_collation(&literal.text)?;
                Ok(LogicalType::varchar_collation(&literal.text))
            }
            // `RegisterFloatConstructors`: FLOAT(precision) is a FLOAT or a DOUBLE
            LogicalTypeId::Float if name == "float" && children.len() == 1 => {
                null_type_parameter("FLOAT", &["precision"], children)?;
                let Some(precision) = type_parameter(&children[0]) else {
                    return Err(binder_error(format!(
                        "Type \"FLOAT\" does not accept type parameters ({})",
                        type_parameter_type(&children[0])
                    )));
                };
                match precision {
                    ..1 => Err(binder_error("precision for type float must be at least 1 bit")),
                    1..=24 => Ok(LogicalType::FLOAT),
                    25..=53 => Ok(LogicalType::DOUBLE),
                    _ => Err(binder_error("precision for type float must be less than 54 bits")),
                }
            }
            // the width of a VARCHAR is accepted and ignored
            LogicalTypeId::Varchar if children.len() <= 1 => Ok(LogicalType::VARCHAR),
            LogicalTypeId::Varchar => {
                let types: Vec<String> = children.iter().map(type_parameter_type).collect();
                Err(binder_error(format!(
                    "Type \"{}\" does not accept type parameters ({})",
                    name.to_uppercase(),
                    types.join(", ")
                )))
            }
            LogicalTypeId::List
            | LogicalTypeId::Array
            | LogicalTypeId::Map
            | LogicalTypeId::Struct
            | LogicalTypeId::Tuple
            | LogicalTypeId::Union => self.bind_nested_type(&name, id, children),
            LogicalTypeId::Enum => {
                let mut values = Vec::with_capacity(children.len());
                for child in children {
                    match &child.kind {
                        ExprKind::Constant(literal) if literal.kind == LiteralKind::String => {
                            values.push(literal.text.clone())
                        }
                        _ => {
                            let types: Vec<String> = children.iter().map(type_parameter_type).collect();
                            return Err(binder_error(format!(
                                "Type \"ENUM\" does not accept type parameters ({})",
                                types.join(", ")
                            )));
                        }
                    }
                }
                if values.is_empty() {
                    return Err(binder_error("ENUM type requires at least one argument"));
                }
                enum_type_from(values)
            }
            // the length of a BIT is accepted and ignored
            LogicalTypeId::Bit if children.len() <= 1 => Ok(LogicalType::new(id)),
            // `BindGeometryType`: the CRS, as the catalog of coordinate systems identifies it
            LogicalTypeId::Geometry if !children.is_empty() => {
                let [child] = children else {
                    let types: Vec<String> = children.iter().map(type_parameter_type).collect();
                    return Err(binder_error(format!(
                        "Type \"GEOMETRY\" does not accept type parameters ({})",
                        types.join(", ")
                    )));
                };
                let text = match &child.kind {
                    ExprKind::Constant(literal) if literal.kind == LiteralKind::String => literal.text.clone(),
                    _ => {
                        return Err(binder_error(format!(
                            "Type \"GEOMETRY\" does not accept type parameters ({})",
                            type_parameter_type(child)
                        )));
                    }
                };
                let identified = duckrs_common::geometry_crs::try_identify(&text)
                    .map_err(|message| Error::new(ErrorKind::InvalidInput, message))?;
                match identified {
                    Some(crs) => Ok(LogicalType::geometry(Some(crs))),
                    None => Err(binder_error(format!(
                        "Encountered unrecognized coordinate system '{text}' when trying to create GEOMETRY type\n\
                         The coordinate system definition may be incomplete or invalid. Your options are as \
                         follows:\n* Load an extension that can identify this coordinate system\n* Provide a full \
                         coordinate system definition in e.g. \"PROJJSON\" or \"WKT2\" format\n* Set the \
                         'ignore_unknown_crs' configuration option to drop the coordinate system from the resulting \
                         geometry type and make this error go away"
                    ))),
                }
            }
            _ if !children.is_empty() => Err(crate::unported(format!("type parameters for {}", name.to_uppercase()))),
            id => Ok(LogicalType::new(id)),
        }
    }

    /// The type constructors of `default_types.cpp` for the nested types.
    fn bind_nested_type(&self, name: &str, id: LogicalTypeId, children: &[Expr]) -> Result<LogicalType> {
        let upper = name.to_uppercase();
        let child_type = |expr: &Expr| self.bind_type(&CastType::Parsed(Box::new(expr.clone())));
        let rejected = || {
            let types: Vec<String> = children.iter().map(type_parameter_type).collect();
            binder_error(format!(
                "Type \"{upper}\" does not accept type parameters ({})",
                types.join(", ")
            ))
        };
        if children.is_empty() && !matches!(id, LogicalTypeId::Struct | LogicalTypeId::Tuple) {
            return Err(binder_error(format!("Type \"{upper}\" requires type parameters")));
        }
        match (id, children) {
            (LogicalTypeId::List, [child]) => Ok(LogicalType::list(child_type(child)?)),
            (LogicalTypeId::Map, [key, value]) => Ok(LogicalType::map(child_type(key)?, child_type(value)?)),
            (LogicalTypeId::Array, [child, size]) => {
                let child = child_type(child)?;
                let size = type_parameter(size).ok_or_else(rejected)?;
                if size < 1 {
                    return Err(binder_error("ARRAY type size must be at least 1"));
                }
                if size > MAX_ARRAY_SIZE {
                    return Err(binder_error(format!(
                        "ARRAY type size must be at most {MAX_ARRAY_SIZE}"
                    )));
                }
                Ok(LogicalType::array(child, size as u32))
            }
            (LogicalTypeId::Struct | LogicalTypeId::Tuple | LogicalTypeId::Union, members) => {
                let members = members
                    .iter()
                    .map(|m| Ok((m.alias.clone(), child_type(m)?)))
                    .collect::<Result<Vec<_>>>()?;
                for (i, (name, _)) in members.iter().enumerate() {
                    if !name.is_empty() && members[..i].iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                        return Err(binder_error(format!(
                            "Duplicate named type parameter {name} for type {upper}"
                        )));
                    }
                }
                let unnamed = members.iter().all(|(n, _)| n.is_empty());
                Ok(match id {
                    LogicalTypeId::Union => {
                        if members.len() > MAX_UNION_MEMBERS {
                            return Err(binder_error(format!(
                                "UNION type supports at most {MAX_UNION_MEMBERS} type modifiers"
                            )));
                        }
                        LogicalType::union_type(members)
                    }
                    _ if unnamed && !members.is_empty() => {
                        LogicalType::tuple(members.into_iter().map(|(_, t)| t).collect())
                    }
                    _ => LogicalType::struct_type(members),
                })
            }
            _ => Err(rejected()),
        }
    }

    /// Whether an aggregate over `function`'s arguments belongs to a query around this one: its
    /// arguments name columns, and all of them are that query's.
    fn aggregate_of_outer_query(&self, arguments: &[Expr]) -> bool {
        let (mut own, mut outer) = (false, false);
        for argument in arguments {
            visit_expr_mut(&mut argument.clone(), &mut |e| {
                // the parameters of a lambda in the arguments are none of the query's columns
                if let ExprKind::Lambda { lhs, expr, .. } = &mut e.kind {
                    let mut parameters = Vec::new();
                    visit_expr_mut(lhs, &mut |p| {
                        if let ExprKind::ColumnRef(names) = &p.kind {
                            parameters.push(names[0].clone());
                        }
                    });
                    visit_expr_mut(expr, &mut |inner| {
                        if let ExprKind::ColumnRef(names) = &inner.kind
                            && parameters.iter().any(|p| p.eq_ignore_ascii_case(&names[0]))
                        {
                            inner.kind = ExprKind::Default;
                        }
                    });
                    **lhs = Expr::from(ExprKind::Default);
                }
                if let ExprKind::ColumnRef(names) = &e.kind {
                    // a USING column reads as ambiguous, and is still that scope's
                    let has = |context: &super::BindContext| {
                        context
                            .bind_column_ref(names)
                            .map_or_else(|error| error.message.starts_with("Ambiguous"), |_| true)
                    };
                    if has(&self.bind_context) {
                        own = true;
                    } else if self.outer_contexts.iter().any(has) {
                        outer = true;
                    }
                }
            });
        }
        outer && !own
    }

    /// `Binder::BindCorrelatedColumns`: a column of a query around this one.
    /// Where the fields of a struct start in `names`: after a column of this query, or else of a
    /// query around it (`ExpressionBinder::BindQualifiedColumnName` on the outer binders).
    fn struct_field_start(&self, names: &[String]) -> Option<usize> {
        if let Some(start) = self.bind_context.struct_field_start(names) {
            return Some(start);
        }
        if self.bind_context.bind_column_ref(names).is_ok() {
            return None;
        }
        self.outer_contexts
            .iter()
            .rev()
            .find_map(|context| context.struct_field_start(names))
    }

    fn bind_outer_column(&mut self, names: &[String]) -> Result<Option<BoundExpr>> {
        for context in self.outer_contexts.iter().rev() {
            let (binding, ty, name) = match context.bind_column_ref(names) {
                Ok(found) => found,
                // a name two tables of that scope have is an error there too, unless they are joined USING it: then
                // it is the column of the table that always has it
                Err(error) if error.message.starts_with("Ambiguous") => {
                    let set = match names {
                        [column] => context.using_binding(column)?.cloned(),
                        _ => None,
                    };
                    let Some(set) = set else {
                        return Err(error);
                    };
                    match &set.primary {
                        Some(alias) => match context.bind_column_ref(&[alias.clone(), names[0].clone()]) {
                            Ok(found) => found,
                            Err(_) => return Err(error),
                        },
                        // a FULL join's USING column is the COALESCE of its tables' columns
                        None => {
                            let expr = set.expression(&names[0]);
                            return self.bind_expression(&expr).map(Some);
                        }
                    }
                }
                Err(_) => continue,
            };
            let Some(binding_of) = context.bindings.iter().find(|b| b.table_index == binding.table_index) else {
                return Ok(None);
            };
            let alias = binding_of.alias.clone();
            let mut correlated = self.correlated.lock();
            if !correlated.iter().any(|c| c.binding == binding) {
                correlated.push(CorrelatedColumn {
                    binding,
                    names: vec![alias, name.clone()],
                });
            }
            let mut expr = BoundExpr::column_ref(binding, ty, name);
            expr.kind = BoundExprKind::ColumnRef { binding, depth: 1 };
            return Ok(Some(expr));
        }
        Ok(None)
    }

    /// A name a subquery could not find may be a correlated column, which is a missing feature
    /// and not a mistake in the query.
    fn correlated_error(&self, error: Error) -> Error {
        let name = error.message.split('"').nth(1).unwrap_or_default();
        let correlated = error.kind == ErrorKind::Binder
            && error.message.starts_with("Referenced")
            && [&self.bind_context]
                .into_iter()
                .chain(&self.outer_contexts)
                .chain(&self.left_of_join)
                .flat_map(|context| &context.bindings)
                .any(|b| b.alias.eq_ignore_ascii_case(name) || b.column_index(name).is_some());
        if correlated {
            crate::unported("correlated subqueries")
        } else {
            error
        }
    }

    /// `ExpressionBinder::BindExpression(SubqueryExpression)`.
    fn bind_subquery(
        &mut self,
        subquery: &ast::SelectStatement,
        subquery_type: SubqueryType,
        child: Option<&Expr>,
        comparison: ComparisonOp,
    ) -> Result<BoundExpr> {
        let parsed_child = child;
        let mut rewritten = subquery.clone();
        // aggregates of this query's columns stand in the subquery under a name of their own
        let mut outer_aggregates: Vec<(String, Expr, LogicalType)> = Vec::new();
        // `BaseSelectBinder::TryBindGroup` at depth > 0: an expression of the subquery that is a
        // group of this query stands for the group
        let group_expressions = std::mem::take(&mut self.subquery_group_expressions);
        if group_expressions && let Some(state) = self.aggregates.as_ref() {
            let keys: Vec<Expr> = state.group_map.iter().map(|(key, _)| key.clone()).collect();
            let mut found: Vec<Expr> = Vec::new();
            // the subqueries in the subquery too: a group stands for itself at any depth
            visit_query_and_subqueries(&mut rewritten.node, &mut |e| {
                // an alias of this query's select list that is a group (`k IN (SELECT ... + k)`)
                let aliased = match &e.kind {
                    ExprKind::ColumnRef(names)
                        if names.len() == 1 && self.bind_context.bind_column_ref(names).is_err() =>
                    {
                        self.alias_fallback
                            .iter()
                            .find(|(alias, _)| alias.eq_ignore_ascii_case(&names[0]))
                            .map(|(_, aliased)| aliased.clone())
                            // a USING column of a FULL join, which is the COALESCE it groups by
                            .or_else(|| self.bind_context.using_expression(&names[0]).ok().flatten())
                    }
                    _ => None,
                };
                if aliased.is_none() && matches!(e.kind, ExprKind::ColumnRef(_) | ExprKind::Constant(_)) {
                    return;
                }
                let mut qualified = aliased.unwrap_or_else(|| e.clone());
                self.qualify_column_names(&mut qualified);
                let qualified = without_aliases(&qualified);
                if let Some(key) = keys.iter().find(|key| **key == qualified) {
                    let at = match found.iter().position(|f| f == key) {
                        Some(at) => at,
                        None => {
                            found.push(key.clone());
                            found.len() - 1
                        }
                    };
                    let alias = std::mem::take(&mut e.alias);
                    *e = Expr::column(vec![format!("__outer_aggregate_group_{at}")]);
                    e.alias = alias;
                }
            });
            for (at, key) in found.into_iter().enumerate() {
                let ty = self.bind_expression(&key)?.return_type;
                outer_aggregates.push((format!("__outer_aggregate_group_{at}"), key, ty));
            }
        }
        let bound_columns = self.aggregates.as_ref().map(|state| state.bound_columns.len());
        let (binder, bound) = loop {
            let mut context = self.bind_context.clone();
            if !outer_aggregates.is_empty() {
                let table_index = self.generate_table_index();
                context.add_binding(super::Binding {
                    alias: "__outer_aggregates".to_string(),
                    schema: None,
                    catalog: None,
                    table_index,
                    names: outer_aggregates.iter().map(|(n, ..)| n.clone()).collect(),
                    types: outer_aggregates.iter().map(|(.., t)| t.clone()).collect(),
                    row_id: None,
                    virtual_columns: Vec::new(),
                    generated: Vec::new(),
                });
            }
            let mut binder = self.child_binder();
            binder.outer_contexts.push(context);
            for (alias, aliased) in &self.alias_fallback {
                let mut aliased = aliased.clone();
                self.qualify_column_names(&mut aliased);
                binder
                    .outer_aliases
                    .push((alias.clone(), aliased, self.later_select_aliases.is_some()));
            }
            binder.correlated = Arc::default();
            match binder.bind_select_statement(&rewritten) {
                Ok(bound) => break (binder, bound),
                Err(error) => {
                    let Some(expr) = binder.pending_outer_aggregate.take() else {
                        return Err(self.correlated_error(error));
                    };
                    self.next_table_index = binder.next_table_index;
                    let bound = self.bind_expression(&expr)?;
                    let name = format!("__outer_aggregate_{}", outer_aggregates.len());
                    let ty = bound.return_type.clone();
                    let mut replaced = false;
                    // where it is, in the subquery or one of its own
                    visit_query_and_subqueries(&mut rewritten.node, &mut |e| {
                        if e.kind == expr.kind {
                            let alias = std::mem::take(&mut e.alias);
                            *e = Expr::column(vec![name.clone()]);
                            e.alias = alias;
                            replaced = true;
                        }
                    });
                    // the aggregate came through an alias of this query (`HavingBinder`): it can't be moved out, and
                    // binding again would find it again
                    if !replaced {
                        let alias = self
                            .alias_fallback
                            .iter()
                            .find(|(_, aliased)| aliased.kind == expr.kind)
                            .map(|(alias, _)| alias.clone());
                        return Err(match alias {
                            Some(alias) => binder_error(format!(
                                "Having clause cannot reference alias \"{alias}\" in correlated subquery"
                            )),
                            None => self.correlated_error(error),
                        });
                    }
                    outer_aggregates.push((name, expr, ty));
                }
            }
        };
        self.next_table_index = binder.next_table_index;
        // the correlated columns are columns of this query, or of one around it
        let columns: Vec<CorrelatedColumn> = binder.correlated.lock().clone();
        let mut correlated = Vec::with_capacity(columns.len());
        let mut bindings = Vec::with_capacity(columns.len());
        let fallback_size = self.alias_fallback.len();
        self.alias_fallback
            .extend(outer_aggregates.iter().map(|(n, e, _)| (n.clone(), e.clone())));
        for column in columns {
            let names = match column.names.as_slice() {
                [alias, name] if alias == "__outer_aggregates" => vec![name.clone()],
                _ => column.names,
            };
            let outer = std::mem::replace(&mut self.binding_correlated, true);
            let bound = self.bind_expression(&Expr::from(ExprKind::ColumnRef(names)));
            self.binding_correlated = outer;
            match bound {
                Ok(bound) => correlated.push(bound),
                Err(error) => {
                    self.alias_fallback.truncate(fallback_size);
                    // in HAVING a column that is no group is an error at once: try again with the expressions of
                    // the groups, as the select list does below
                    if !group_expressions
                        && error.message.contains("must appear in the GROUP BY clause")
                        && self.aggregates.as_ref().is_some_and(|state| !state.group_map.is_empty())
                    {
                        self.subquery_group_expressions = true;
                        let retried = self.bind_subquery(subquery, subquery_type, parsed_child, comparison);
                        self.subquery_group_expressions = false;
                        if retried.is_ok() {
                            return retried;
                        }
                    }
                    return Err(error);
                }
            }
            bindings.push(column.binding);
        }
        self.alias_fallback.truncate(fallback_size);
        // a correlated column that is no group: try again with the expressions of the groups
        if !group_expressions
            && let (Some(before), Some(state)) = (bound_columns, self.aggregates.as_mut())
            && state.bound_columns.len() > before
            && !state.group_map.is_empty()
        {
            let added = state.bound_columns.split_off(before);
            self.subquery_group_expressions = true;
            let retried = self.bind_subquery(subquery, subquery_type, parsed_child, comparison);
            self.subquery_group_expressions = false;
            if retried.is_ok() {
                return retried;
            }
            self.aggregates
                .as_mut()
                .expect("checked above")
                .bound_columns
                .extend(added);
        }
        let child = child.map(|c| self.bind_expression(c)).transpose()?;
        let columns = bound.types.len();
        let row_width = child
            .as_ref()
            .filter(|c| c.return_type.id() == LogicalTypeId::Tuple)
            .map(|c| c.return_type.struct_children().len());
        if subquery_type != SubqueryType::Exists && subquery_type != SubqueryType::NotExists {
            match row_width {
                Some(width) if columns == width && width > 0 && bound.types[0].id() != LogicalTypeId::Tuple => {
                    if !bindings.is_empty() {
                        return Err(crate::error_not_implemented(
                            "Correlated IN/ANY/ALL with multiple columns not yet supported",
                        ));
                    }
                    let names: Vec<String> = (0..width).map(|i| format!("__subquery_column_{i}")).collect();
                    let row = FunctionExpr::new("row", names.iter().map(|n| Expr::column(vec![n.clone()])).collect());
                    let mut from = ast::TableRef::subquery(rewritten);
                    from.alias = "__subquery_row".to_string();
                    from.column_name_alias = names;
                    let select = ast::SelectNode {
                        select_list: vec![row.into()],
                        from_table: from,
                        ..ast::SelectNode::default()
                    };
                    let node: ast::QueryNode = select.into();
                    let wrapped = ast::SelectStatement { node };
                    self.row_subquery = true;
                    return self.bind_subquery(&wrapped, subquery_type, parsed_child, comparison);
                }
                Some(width) if bound.types[0].id() != LogicalTypeId::Tuple || columns != 1 => {
                    return Err(binder_error(format!(
                        "Subquery returns {columns} columns - expected {width}"
                    )));
                }
                None if columns != 1 => {
                    return Err(binder_error(format!("Subquery returns {columns} columns - expected 1")));
                }
                _ => {}
            }
        }
        let (kind, return_type, plan, child) = match subquery_type {
            SubqueryType::Scalar => (SubqueryKind::Scalar, bound.types[0].clone(), bound.plan, None),
            SubqueryType::Exists | SubqueryType::NotExists => {
                (SubqueryKind::Exists, LogicalType::BOOLEAN, bound.plan, None)
            }
            SubqueryType::Any => {
                let Some(child) = child else {
                    return Err(crate::unported("ANY without a value to compare"));
                };
                // `IsMultiColumnComparison`: other than `=`, a correlated comparison of rows of
                // several fields is not planned (`<`, `>` and friends are rewritten to MIN/MAX first)
                let multi_column_row =
                    |ty: &LogicalType| ty.id() == LogicalTypeId::Tuple && ty.struct_children().len() > 1;
                if !bindings.is_empty()
                    && comparison == ComparisonOp::NotEqual
                    && (multi_column_row(&child.return_type) || multi_column_row(&bound.types[0]))
                {
                    return Err(crate::error_not_implemented(
                        "Correlated IN/ANY/ALL with multiple columns not yet supported",
                    ));
                }
                let child_type = expression_return_type(&child);
                let Some(compare_type) = try_max_logical_type(&child_type, &bound.types[0]) else {
                    return Err(binder_error(format!(
                        "Cannot compare values of type {} and {} in IN/ANY/ALL clause - an explicit cast is required",
                        child.return_type, bound.types[0]
                    )));
                };
                let compare_type = normalize_type(&compare_type);
                let child = checked_cast(child, &compare_type)?;
                let mut plan = self.cast_plan(bound, std::slice::from_ref(&compare_type), vec![Some(0)]);
                // both sides compare with the collation of the type they compare in (`PlanSubqueries`)
                let child = self.collate(child, &compare_type)?;
                let column = BoundExpr::column_ref(plan.column_bindings()[0], compare_type.clone(), String::new());
                let collated = self.collate(column, &compare_type)?;
                if !matches!(collated.kind, BoundExprKind::ColumnRef { .. }) {
                    plan = crate::planner::logical::LogicalOperator::new(
                        crate::planner::logical::LogicalOperatorKind::Projection {
                            table_index: self.generate_table_index(),
                            expressions: vec![collated],
                        },
                        vec![plan],
                    );
                }
                (
                    SubqueryKind::Any(comparison),
                    LogicalType::BOOLEAN,
                    plan,
                    Some(Box::new(child)),
                )
            }
        };
        let mut plan = plan;
        flatten_subqueries(&mut plan, &mut self.next_table_index);
        let raw = plan.clone();
        let mut plan = optimize(plan, self.options.optimizer);
        resolve_column_bindings(&mut plan)?;
        let volatile = plan.is_volatile();
        let expression = BoundExpr::new(
            BoundExprKind::Subquery {
                subquery: Arc::new(BoundSubquery {
                    kind,
                    plan,
                    // flattening leaves a volatile one alone; `EXPLAIN (SQL)` reads it
                    raw: Some(raw),
                    correlated: bindings,
                    volatile,
                    error_on_multiple_rows: self.options.scalar_subquery_error,
                    row: std::mem::take(&mut self.row_subquery),
                    results: Mutex::default(),
                }),
                child,
                correlated,
            },
            return_type,
        );
        Ok(if subquery_type == SubqueryType::NotExists {
            BoundExpr::new(
                BoundExprKind::Operator {
                    op: BoundOperator::Not,
                    children: vec![expression],
                },
                LogicalType::BOOLEAN,
            )
        } else {
            expression
        })
    }

    /// `ExpressionBinder::Bind`.
    pub(super) fn bind_expression(&mut self, expr: &Expr) -> Result<BoundExpr> {
        // `TryRewriteQualifiedCountStar`, wherever an aggregate can be
        if super::star::is_qualified_count_star(expr) {
            let mut rewritten = expr.clone();
            self.rewrite_qualified_count_star(&mut rewritten);
            return self.bind_expression(&rewritten);
        }
        // a comparison operator called as a function is the comparison
        if let ExprKind::Function(function) = &expr.kind
            && function.name.qualification.iter().all(String::is_empty)
            && let [left, right] = function.args.as_slice()
            && left.name.is_empty()
            && right.name.is_empty()
            && !function.distinct
            && function.order_by.is_empty()
            && function.filter.is_none()
        {
            use duckrs_parser::ast::ComparisonOp as C;
            let op = match function.name.name.to_uppercase().as_str() {
                "=" | "==" => Some(C::Equal),
                "!=" | "<>" => Some(C::NotEqual),
                "<" => Some(C::LessThan),
                ">" => Some(C::GreaterThan),
                "<=" => Some(C::LessThanOrEqual),
                ">=" => Some(C::GreaterThanOrEqual),
                "IS DISTINCT FROM" => Some(C::DistinctFrom),
                "IS NOT DISTINCT FROM" => Some(C::NotDistinctFrom),
                _ => None,
            };
            if let Some(op) = op {
                let mut comparison = Expr::comparison(op, left.expr.clone(), right.expr.clone());
                comparison.alias = if expr.alias.is_empty() {
                    expr.to_string()
                } else {
                    expr.alias.clone()
                };
                return self.bind_expression(&comparison);
            }
        }
        // `BindHasError`: the built-in has_error is the HAS_ERROR operator over its argument
        if let ExprKind::Function(function) = &expr.kind
            && function.name.name.eq_ignore_ascii_case("has_error")
            && self.is_builtin_has_error(&function.name)?
        {
            if function.distinct || function.filter.is_some() || !function.order_by.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Function has_error is a Scalar Function. \"DISTINCT\", \"FILTER\", and \"ORDER BY\" are \
                     only applicable to window and aggregate functions.",
                ));
            }
            if function.args.len() != 1 {
                return Err(binder_error(
                    "Wrong number of arguments provided to HAS_ERROR expression",
                ));
            }
            if !function.args[0].name.is_empty() {
                return Err(binder_error(
                    "Named arguments are not supported in HAS_ERROR expression",
                ));
            }
            // `TryBindSubqueryErrors`: ORDER BY and DISTINCT of a subquery leave no room for the error flag
            let mut modified = false;
            duckrs_parser::transformer::visit_expr_mut(&mut function.args[0].expr.clone(), &mut |e: &mut Expr| {
                if let ExprKind::Subquery { subquery, .. } = &e.kind {
                    modified |= subquery
                        .node
                        .modifiers
                        .iter()
                        .any(|m| matches!(m, ast::ResultModifier::Order(_) | ast::ResultModifier::Distinct(_)));
                }
            });
            if modified {
                return Err(binder_error(
                    "TRY over a subquery with result modifiers is only supported when the subquery is the direct \
                     argument of TRY",
                ));
            }
            let outer = std::mem::replace(&mut self.inside_try, true);
            let child = crate::function::binder::inside_try(|| self.bind_expression(&function.args[0].expr));
            self.inside_try = outer;
            let child = child?;
            if child.is_volatile() {
                return Err(binder_error(
                    "HAS_ERROR can not be used in combination with a volatile function",
                ));
            }
            let mut bound = BoundExpr::new(
                BoundExprKind::Operator {
                    op: BoundOperator::HasError,
                    children: vec![child],
                },
                LogicalType::BOOLEAN,
            );
            if !expr.alias.is_empty() {
                bound = bound.with_alias(expr.alias.clone());
            }
            return Ok(bound);
        }
        if let ExprKind::Function(function) = &expr.kind
            && function.name.name.eq_ignore_ascii_case("switch")
            && function.name.qualification.iter().all(String::is_empty)
            && (1..=3).contains(&function.args.len())
        {
            return self.bind_switch(function);
        }
        if matches!(expr.kind, ExprKind::Window(_)) {
            let mut rewritten = expr.clone();
            if self.rewrite_window_count_star(&mut rewritten) {
                return self.bind_expression(&rewritten);
            }
        }
        // `ExpressionBinder::TryBindLambdaOrJson`: a function that takes no lambda and is given an
        // arrow binds the arrow as JSON's `->`; when that fails too, both failures are told
        let arrow_argument = |e: &Expr| {
            matches!(
                e.kind,
                ExprKind::Lambda {
                    syntax: ast::LambdaSyntax::Arrow,
                    ..
                }
            )
        };
        let not_lambda_call = match &expr.kind {
            ExprKind::Function(function) => {
                !super::lambda::is_lambda_function(&function.name.name)
                    && function.args.iter().any(|a| arrow_argument(&a.expr))
            }
            ExprKind::Operator {
                op: ast::OperatorType::ArrayConstructor,
                children,
            } => children.iter().any(arrow_argument),
            _ => false,
        };
        let bound = match self.bind_expression_inner(expr) {
            Err(error) if not_lambda_call && error.kind == ErrorKind::Binder => {
                return Err(binder_error(format!(
                    "failed to bind function, either: This scalar function does not support lambdas!\n or: {}",
                    error.message
                )));
            }
            bound => bound?,
        };
        // `BindMapContainsExpression`: with a collation on the key or the map's keys, the key is
        // looked for among the keys as a list: list_contains(map_keys(m), key)
        if let (ExprKind::Function(call), BoundExprKind::Function { function, children, .. }) =
            (&expr.kind, &bound.kind)
            && function.name == "map_contains"
            && let [map, key] = children.as_slice()
            && let [map_argument, key_argument] = call.args.as_slice()
        {
            fn has_collation(ty: &LogicalType) -> bool {
                match ty.id() {
                    LogicalTypeId::Varchar => ty.alias().is_none() && ty.collation().is_some_and(|c| !c.is_empty()),
                    LogicalTypeId::List | LogicalTypeId::Array => ty.child_type().is_some_and(has_collation),
                    _ => false,
                }
            }
            let map_key = (map.return_type.id() == LogicalTypeId::Map)
                .then(|| {
                    map.return_type
                        .child_type()
                        .and_then(|entry| entry.struct_children().first().map(|(_, t)| t.clone()))
                })
                .flatten();
            if has_collation(&key.return_type) || map_key.as_ref().is_some_and(has_collation) {
                let keys = Expr::from(ExprKind::Function(ast::FunctionExpr::new(
                    "map_keys",
                    vec![map_argument.expr.clone()],
                )));
                let mut rewritten = ast::FunctionExpr::new("list_contains", vec![keys, key_argument.expr.clone()]);
                rewritten.name.qualification.clear();
                let mut rewritten = Expr::from(ExprKind::Function(rewritten));
                rewritten.alias.clone_from(&expr.alias);
                return self.bind_expression(&rewritten);
            }
        }
        if matches!(expr.kind, ExprKind::Function(_) | ExprKind::Operator { .. })
            && matches!(bound.kind, BoundExprKind::Function { .. })
        {
            return self.function_collations(bound);
        }
        Ok(bound)
    }

    /// Whether `name` resolves to the built-in has_error: a macro of the same name shadows it, unless
    /// the system catalog is named.
    fn is_builtin_has_error(&self, name: &ast::QualifiedName) -> Result<bool> {
        let parts: Vec<&str> = name
            .qualification
            .iter()
            .filter(|q| !q.is_empty())
            .map(String::as_str)
            .collect();
        if parts.first().is_some_and(|c| c.eq_ignore_ascii_case("system")) {
            return Ok(true);
        }
        if !matches!(parts.as_slice(), [] | ["main"]) {
            return Ok(false);
        }
        Ok(self.find_macro(name, false)?.is_none())
    }

    /// `SwitchFun`: `switch([key,] cases [, default])` over a constant MAP of cases, as the CASE
    /// expression `SwitchBindExpression` makes of it.
    fn bind_switch(&mut self, function: &FunctionExpr) -> Result<BoundExpr> {
        let arguments = function
            .args
            .iter()
            .map(|a| self.bind_expression(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        let is_map = |e: &BoundExpr| e.return_type.id() == LogicalTypeId::Map;
        let map_index = match arguments.len() {
            3 => 1,
            2 if is_map(&arguments[1]) => 1,
            _ => 0,
        };
        if !is_map(&arguments[map_index]) {
            let types: Vec<String> = arguments
                .iter()
                .map(|a| crate::function::binder::expression_return_type(a).to_string())
                .collect();
            return Err(binder_error(format!(
                "No function matches the given name and argument types 'switch({})'. You might need to add \
                 explicit type casts.",
                types.join(", ")
            )));
        }
        let cases = &arguments[map_index];
        if !cases.is_foldable() {
            return Err(binder_error("SWITCH expected a constant map for the cases"));
        }
        let cases_value = evaluate_scalar(cases)?;
        let ValueData::List(entries) = cases_value.data() else {
            return Err(binder_error("SWITCH expected a constant map for the cases"));
        };
        if entries.is_empty() {
            return Err(binder_error("No values provided for SWITCH expression"));
        }
        let parts = cases
            .return_type
            .child_type()
            .expect("an entry type")
            .struct_children()
            .to_vec();
        let return_type = parts[1].1.clone();
        let base = (map_index == 1).then(|| arguments[0].clone());
        // the template `K` of the key and the cases' keys
        if let Some(base) = &base {
            let (key_type, case_type) = (
                crate::function::binder::expression_return_type(base),
                parts[0].1.clone(),
            );
            if crate::function::nested::deduce_template(&key_type, &case_type).is_none() {
                return Err(binder_error(format!(
                    "No function matches the given name and argument types 'switch({key_type}, {}, ..)'. You might \
                     need to add explicit type casts.\n\tCannot deduce template type 'K', it was inferred to be:\n  \
                     - '{key_type}', from first occurrence\n  - '{case_type}', which is incompatible with the \
                     previously inferred type",
                    cases.return_type
                )));
            }
        }
        let mut checks = Vec::with_capacity(entries.len());
        for entry in entries {
            let ValueData::Struct(fields) = entry.data() else {
                continue;
            };
            let key = BoundExpr::constant(fields[0].clone());
            let when = match &base {
                Some(base) => bind_comparison(ComparisonOp::Equal, base.clone(), key)?,
                None => key.cast_to(&LogicalType::BOOLEAN, false),
            };
            checks.push((when, BoundExpr::constant(fields[1].clone())));
        }
        let else_expr = match arguments.get(map_index + 1) {
            Some(default) => default.clone().cast_to(&return_type, false),
            None => BoundExpr::constant(Value::null(return_type.clone())),
        };
        Ok(BoundExpr::new(
            BoundExprKind::Case {
                checks,
                else_expr: Box::new(else_expr),
            },
            return_type,
        ))
    }

    /// `PushArgumentCollations` and `PropagateReturnCollation` of `FunctionBinder`: the
    /// functions that compare their arguments (LIKE, `contains`, ...) see them with their
    /// combinable collations; the others give a VARCHAR with the collation of their arguments.
    fn function_collations(&self, mut bound: BoundExpr) -> Result<BoundExpr> {
        use crate::planner::collation::{CollationType, push_collation};
        let BoundExprKind::Function { function, children, .. } = &mut bound.kind else {
            return Ok(bound);
        };
        // `ListSortBindData`: the elements sort by their collated values, a list of them after the
        // options, and keep their own
        let grade_up = function.name == "list_grade_up";
        if (grade_up || matches!(function.name.as_str(), "list_sort" | "array_sort")) && children.len() <= 3 {
            // `ListNormalSortBind`: `ResolveOrder` and `ResolveNullOrder` fill in the options
            // that are not given, from the settings
            let option = |text: &str| BoundExpr::constant(Value::varchar(text));
            let given = match children.get(1).map(|c| &c.kind) {
                None => Some(ast::OrderType::Default),
                Some(BoundExprKind::Constant(value)) if !value.is_null() => {
                    match value.try_to_string().unwrap_or_default().to_uppercase().as_str() {
                        "ASC" => Some(ast::OrderType::Ascending),
                        "DESC" => Some(ast::OrderType::Descending),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(order) = given
                && children.len() < 3
            {
                let modifiers = self.options.resolve_order(order, ast::NullOrder::Default);
                if children.len() < 2 {
                    children.push(option(match modifiers.order {
                        duckrs_common::sort_key::OrderType::Descending => "DESC",
                        _ => "ASC",
                    }));
                }
                children.push(option(match modifiers.nulls {
                    duckrs_common::sort_key::NullOrder::NullsLast => "NULLS LAST",
                    _ => "NULLS FIRST",
                }));
            }
            // `ListGradeUpBind`: the same options; the grades are of the values as they are
            if grade_up {
                return Ok(bound);
            }
            let ty = children[0].return_type.clone();
            let (keys, collated) = push_collation(
                children[0].clone(),
                &ty,
                &self.default_collation(),
                CollationType::AllCollations,
            )?;
            if collated {
                if children.len() < 2 {
                    children.push(option("ASC"));
                }
                if children.len() < 3 {
                    children.push(option("NULLS FIRST"));
                }
                children.push(keys);
            }
            return Ok(bound);
        }
        let pushes = PUSH_COMBINABLE_COLLATIONS.contains(&function.name.to_lowercase().as_str());
        let varchar = |ty: &LogicalType| ty.id() == LogicalTypeId::Varchar && ty.alias().is_none();
        // `ExtractCollation`, for a function that gives a VARCHAR (`PropagateReturnCollation`)
        let mut collation: Option<String> = None;
        if !pushes && varchar(&bound.return_type) {
            for child in children.iter().filter(|c| varchar(&c.return_type)) {
                match (&collation, child.return_type.collation()) {
                    (Some(c), Some(other)) if c != other => {
                        return Err(binder_error("Cannot combine types with different collation!"));
                    }
                    (None, Some(other)) => collation = Some(other.to_string()),
                    _ => {}
                }
            }
        }
        if pushes {
            // `PushCollations`: the collations of all arguments, nested ones too, go to each of them
            use crate::planner::collation::{FunctionCollations, apply_collations, extract_collations};
            let mut collations = FunctionCollations::default();
            for child in children.iter() {
                extract_collations(&child.return_type, &mut collations)?;
            }
            let default = self.default_collation();
            for child in children.iter_mut() {
                let ty = apply_collations(&child.return_type, &collations);
                let taken = std::mem::replace(child, BoundExpr::constant(Value::sql_null()));
                *child = push_collation(taken, &ty, &default, CollationType::CombinableCollations)?.0;
            }
        } else if varchar(&bound.return_type)
            && let Some(collation) = collation
        {
            bound.return_type = bound.return_type.with_collation(Some(&collation));
        }
        Ok(bound)
    }

    /// Whether `expr` is an untyped parameter of the macro being created, which binds as
    /// UNKNOWN (`DummyBinding`).
    fn is_unknown_macro_parameter(&self, expr: &Expr) -> bool {
        matches!(&expr.kind, ExprKind::ColumnRef(names)
            if names.len() == 2
                && names[0] == super::macros::CREATE_PARAMETER_MARK
                && names[1].parse::<usize>().ok().and_then(|i| self.macro_arguments.get(i))
                    .is_some_and(|(_, argument)| matches!(argument.kind, ExprKind::Constant(_))))
    }

    /// `ColumnQualifier::CreateStructPack`: a name that is a table of the FROM clause, as
    /// `[schema.]table`, is a STRUCT of its columns.
    fn implicit_struct_pack(&self, names: &[String]) -> Option<Expr> {
        // `[catalog.][schema path.]table` (`GetTableQualifications`): the binding's catalog and
        // schema path end in what qualifies the table
        let (table, qualification) = names.split_last()?;
        let schema =
            (!qualification.is_empty()).then(|| qualification.join(&crate::catalog::SCHEMA_PATH_SEPARATOR.to_string()));
        let own = self.bind_context.bindings_of(schema.as_deref(), table).ok();
        // a table of a query around this one, when no scope has a column of that name (a LATERAL's left side)
        let outer = || {
            if schema.is_some() || self.outer_contexts.iter().any(|c| c.bind_column_ref(names).is_ok()) {
                return None;
            }
            self.outer_contexts
                .iter()
                .rev()
                .find_map(|context| context.bindings_of(None, table).ok())
        };
        let bindings = match own {
            Some(bindings) => bindings,
            None => outer()?,
        };
        let [binding] = bindings.as_slice() else {
            return None;
        };
        let args = binding
            .names
            .iter()
            .map(|column| {
                // by the schema (and catalog) too, where the alias alone names more than one table
                let path = if schema.is_some() {
                    self.bind_context.qualified_names(binding, column)
                } else {
                    vec![binding.alias.clone(), column.clone()]
                };
                ast::FunctionArgument {
                    name: column.clone(),
                    expr: Expr::column(path),
                }
            })
            .collect();
        let mut function = ast::FunctionExpr::new("struct_pack", Vec::new());
        function.args = args;
        Some(Expr::from(ExprKind::Function(function)))
    }

    /// `SelectBinder::TryResolveAliasReference` / `ColumnAliasBinder::BindAlias`: the select
    /// list entry that `name` is the alias of, if there is one before it.
    fn try_bind_alias_reference(&mut self, name: &str) -> Result<Option<BoundExpr>> {
        let Some(position) = self
            .alias_fallback
            .iter()
            .position(|(a, _)| a.eq_ignore_ascii_case(name))
        else {
            if self
                .later_select_aliases
                .as_ref()
                .is_some_and(|later| later.iter().any(|a| a.eq_ignore_ascii_case(name)))
            {
                return Err(binder_error(format!(
                    "Column \"{name}\" referenced that exists in the SELECT clause - but this column cannot be \
                     referenced before it is defined"
                )));
            }
            // an alias of a query around this one
            let Some(position) = self
                .outer_aliases
                .iter()
                .rposition(|(a, ..)| a.eq_ignore_ascii_case(name))
            else {
                return Ok(None);
            };
            let (alias, aliased, select_list) = self.outer_aliases.remove(position);
            if select_list && super::pivot::has_subquery(&aliased) {
                return Err(alias_subquery_error(name));
            }
            let bound = self.bind_expression(&aliased);
            self.outer_aliases.insert(position, (alias, aliased, select_list));
            return bound.map(Some);
        };
        if self.later_select_aliases.is_some() && super::pivot::has_subquery(&self.alias_fallback[position].1) {
            return Err(alias_subquery_error(name));
        }
        // an alias may not refer to itself
        let (alias, aliased) = self.alias_fallback.remove(position);
        let bound = self.bind_expression(&aliased);
        self.alias_fallback.insert(position, (alias.clone(), aliased));
        let bound = bound?;
        // `SelectBindState::BindAlias`: a copy of a volatile expression would be another value
        if bound.is_volatile() {
            return Err(binder_error(format!(
                "Alias \"{alias}\" referenced - but the expression has side effects. This is not yet supported."
            )));
        }
        Ok(Some(bound))
    }

    fn bind_expression_inner(&mut self, expr: &Expr) -> Result<BoundExpr> {
        if let Some(group) = self.try_bind_group(expr) {
            return Ok(group);
        }
        // `ParameterNotResolvedException`: what an untyped macro parameter goes into can't be
        // bound before the macro is called, and binding the macro waits for it
        if !self.macro_arguments.is_empty() {
            let children: Vec<&Expr> = match &expr.kind {
                ExprKind::Function(function) => function.args.iter().map(|a| &a.expr).collect(),
                ExprKind::Operator { children, .. } | ExprKind::Conjunction { children, .. } => {
                    children.iter().collect()
                }
                ExprKind::Comparison { left, right, .. } => vec![left, right],
                ExprKind::Between { input, lower, upper } => vec![input, lower, upper],
                ExprKind::Collate { child, .. } => vec![child],
                _ => Vec::new(),
            };
            if children.iter().any(|child| self.is_unknown_macro_parameter(child)) {
                // a macro it calls is still read: noted for the dependencies of the macro being created
                if let ExprKind::Function(function) = &expr.kind {
                    self.find_macro(&function.name, false)?;
                }
                // the arguments bind first: a column that isn't there is the error
                for child in children {
                    if !matches!(child.kind, ExprKind::Lambda { .. }) {
                        self.bind_expression(child)?;
                    }
                }
                return Err(Error::new(
                    ErrorKind::ParameterNotResolved,
                    "Could not determine the types of the macro parameters",
                ));
            }
        }
        if let ExprKind::Function(function) = &expr.kind
            && let Some(entry) = self.find_macro(&function.name, false)?
        {
            return self.bind_macro(function, &entry);
        }
        if let ExprKind::Function(function) = &expr.kind
            && function.name.name.to_ascii_lowercase().starts_with("json_group_")
        {
            let args: Vec<String> = function.args.iter().map(|a| format!("({})", a.expr)).collect();
            if let Some(text) = crate::function::json::macro_text(&function.name.name, &args) {
                let select = super::table_functions::parse_select(&format!("SELECT {text}"), "Expected an expression")?;
                if let ast::QueryNodeKind::Select(node) = &select.node.kind
                    && let Some(body) = node.select_list.first()
                {
                    return self.bind_expression(body);
                }
            }
        }
        let bound = match &expr.kind {
            ExprKind::Constant(literal) => BoundExpr::constant(literal_to_value(literal)?),
            // outside of a function that takes a lambda, `a -> b` is the JSON operator
            ExprKind::Lambda {
                lhs,
                expr,
                syntax: syntax @ (ast::LambdaSyntax::Arrow | ast::LambdaSyntax::StoredArrow),
            } => {
                // `RestructureArrowChain`: the grammar makes `A -> B ->> C` into `A -> (B ->> C)`, which is
                // `(A -> B) ->> C`
                if let ExprKind::Function(function) = &expr.kind
                    && function.name.qualification.iter().all(String::is_empty)
                    && function.name.name == "->>"
                    && function.args.len() == 2
                {
                    let inner = ast::Expr::from(ExprKind::Lambda {
                        lhs: lhs.clone(),
                        expr: Box::new(function.args[0].expr.clone()),
                        syntax: *syntax,
                    });
                    let mut outer = function.clone();
                    outer.args[0].expr = inner;
                    return self.bind_expression(&ast::Expr::from(ExprKind::Function(outer)));
                }
                let children = vec![self.bind_expression(lhs)?, self.bind_expression(expr)?];
                let types = std::cell::RefCell::new(self.child_binder());
                return crate::function::json::bind_function("->", children, &|name| types.borrow_mut().csv_type(name));
            }
            // a lambda's parameter is one before a name of a table function's argument is its text
            ExprKind::ColumnRef(names) if !self.lambda_scopes.is_empty() && self.is_lambda_parameter(names) => {
                return Ok(self.bind_lambda_parameter(names)?.expect("just checked"));
            }
            ExprKind::ColumnRef(names) if self.table_function_argument => {
                // `TableFunctionBinder::BindColumnReference`: a column a query around binds is a lateral parameter,
                // which a table function does not take; a name no scope binds is its text
                if !self.table_function_name.is_empty()
                    && self.outer_contexts.iter().any(|c| c.bind_column_ref(names).is_ok())
                {
                    return Err(binder_error(format!(
                        "Table function \"{}\" does not support lateral join column parameters - cannot use column \
                         \"{}\" in this context.\nThe function only supports literals as parameters.",
                        self.table_function_name,
                        names.join(".")
                    )));
                }
                BoundExpr::constant(Value::varchar(names.join(".")))
            }
            ExprKind::ColumnRef(names) if self.struct_field_start(names).is_some() => {
                let start = self.struct_field_start(names).expect("checked by the guard");
                let column = ast::Expr::from(ExprKind::ColumnRef(names[..start].to_vec()));
                let mut bound = self.bind_expression(&column)?;
                for field in &names[start..] {
                    // `STRUCT_EXTRACT` of a MAP is `map_extract_value`
                    if bound.return_type.id() == LogicalTypeId::Map {
                        let key = BoundExpr::constant(Value::varchar(field.clone()));
                        bound = crate::function::extras::bind_function("map_extract_value", &[], vec![bound, key])?;
                        continue;
                    }
                    if !matches!(
                        bound.return_type.id(),
                        LogicalTypeId::Struct
                            | LogicalTypeId::Tuple
                            | LogicalTypeId::Union
                            | LogicalTypeId::Variant
                            | LogicalTypeId::Geometry
                    ) {
                        return Err(binder_error(format!(
                            "Cannot extract field '{field}' from expression \"{}\" because it is not a struct",
                            names[..start].join(".")
                        )));
                    }
                    let key = BoundExpr::constant(Value::varchar(field.clone()));
                    // `v.key` of a VARIANT is its variant_extract, which struct_extract itself refuses
                    let name = if bound.return_type.id() == LogicalTypeId::Variant {
                        "array_extract"
                    } else {
                        "struct_extract"
                    };
                    let children = vec![bound, key];
                    bound = bind_nested_function(name, vec![String::new(); 2], children)?;
                }
                bound.with_alias(names.last().expect("a name").clone())
            }
            ExprKind::ColumnRef(names)
                if names.len() == 1
                    && sql_value_function(&names[0]).is_some()
                    && self.bind_context.bind_column_ref(names).is_err() =>
            {
                let function = sql_value_function(&names[0]).expect("just checked");
                let call = crate::storage::writer::parse_expression(&format!("{function}()"))?;
                return self.bind_expression(&call);
            }
            // `SetupRowScope`: NEW.column or OLD.column of the row of a FOR EACH ROW trigger
            ExprKind::ColumnRef(names)
                if names.len() <= 2
                    && self.bind_context.bind_column_ref(names).is_err()
                    && !self.outer_contexts.iter().any(|c| c.bind_column_ref(names).is_ok())
                    && let Some(value) = crate::planner::trigger_scope::row_value(names) =>
            {
                return Ok(BoundExpr::constant(value).with_alias(names.last().expect("a name").clone()));
            }
            ExprKind::ColumnRef(names)
                if names.len() == 2
                    && names[0] == super::macros::CREATE_PARAMETER_MARK
                    && !self.macro_arguments.is_empty() =>
            {
                // `ColumnQualifier::QualifyColumnName`: a parameter can't also be a column here
                let (name, argument) =
                    self.macro_arguments[names[1].parse::<usize>().expect("a marked position")].clone();
                if !self.bind_context.matching_bindings(&name).is_empty() {
                    return Err(binder_error(format!("Conflicting column names for column {name}!")));
                }
                return self.bind_expression(&argument);
            }
            // `IsPotentialAlias`: `alias.x` names the select list alias `x` when no table is
            // called "alias"
            ExprKind::ColumnRef(names)
                if names.len() == 2 && names[0] == "alias" && self.bind_context.bind_column_ref(names).is_err() =>
            {
                if let Some(bound) = self.try_bind_alias_reference(&names[1])? {
                    return Ok(bound);
                }
                let error = self.bind_context.bind_column_ref(names).expect_err("checked above");
                return Err(self.lateral_error(names, error));
            }
            ExprKind::ColumnRef(names)
                if !names.is_empty()
                    && self.bind_context.bind_column_ref(names).is_err()
                    && let Some(pack) = self.implicit_struct_pack(names) =>
            {
                return self.bind_expression(&pack);
            }
            ExprKind::ColumnRef(names) => {
                let mode = self
                    .aggregates
                    .as_ref()
                    .filter(|state| !state.inside_aggregate)
                    .map(|state| state.mode);
                // an ambiguous name is this query's too, and an error
                let is_own = self
                    .bind_context
                    .bind_column_ref(names)
                    .map_or_else(|error| error.message.starts_with("Ambiguous"), |_| true)
                    || (names.len() == 1
                        && self
                            .alias_fallback
                            .iter()
                            .any(|(a, _)| a.eq_ignore_ascii_case(&names[0])));
                if !is_own && let Some(outer) = self.bind_outer_column(names)? {
                    return Ok(outer);
                }
                if mode == Some(ColumnMode::Having) {
                    return self.bind_having_column(names);
                }
                if let [column] = names.as_slice()
                    && let Some(using) = self.bind_context.using_expression(column)?
                {
                    return self.bind_expression(&using);
                }
                if self.bind_context.bind_column_ref(names).is_ok()
                    && let Some(generated) = self.bind_context.generated_expression(names)
                {
                    return self.bind_expression(&generated);
                }
                let (binding, ty, name) = match names.as_slice() {
                    [column] => match self.bind_context.bind_column(column) {
                        Ok(bound) => bound,
                        Err(error) => {
                            // a column that is not in the FROM clause may name a select list alias
                            if let Some(bound) = self.try_bind_alias_reference(column)? {
                                return Ok(bound);
                            }
                            // `BindContext::BindColumn` looking outward: a query without a FROM clause names the
                            // columns of the nearest query around it that has some
                            let error = match self.outer_contexts.iter().rev().find(|c| !c.bindings.is_empty()) {
                                Some(outer) if self.bind_context.bindings.is_empty() => binder_error(format!(
                                    "Referenced column \"{column}\" not found in FROM clause!{}",
                                    duckrs_common::string_util::candidates_message(
                                        &outer.similar_bindings(column),
                                        "Candidate bindings"
                                    )
                                )),
                                _ => error,
                            };
                            return Err(self.lateral_error(names, error));
                        }
                    },
                    names => self
                        .bind_context
                        .bind_column_ref(names)
                        .map_err(|error| self.lateral_error(names, error))?,
                };
                if mode == Some(ColumnMode::SelectList) {
                    self.aggregates.as_mut().unwrap().bound_columns.push(name.clone());
                }
                if let Some(order) = self.column_orders.get(&binding.table_index) {
                    order.note(binding.column_index);
                }
                BoundExpr::column_ref(binding, ty, name)
            }
            ExprKind::Positional(position) => {
                let names = self.bind_context.position_to_column(*position)?;
                let column = Expr::from(ExprKind::ColumnRef(names));
                return self.bind_expression(&column);
            }
            ExprKind::Comparison { op, left, right } => {
                let left = self.bind_expression(left)?;
                let right = self.bind_expression(right)?;
                let originals = [left.return_type.clone(), right.return_type.clone()];
                let mut bound = bind_comparison(*op, left, right)?;
                if let BoundExprKind::Comparison { left, right, .. } = &mut bound.kind {
                    let sides = vec![(**left).clone(), (**right).clone()];
                    let [l, r]: [BoundExpr; 2] =
                        self.collate_compared(&originals, sides)?.try_into().expect("two sides");
                    (**left, **right) = (l, r);
                }
                bound
            }
            // `BindExpression(CollateExpression)`
            ExprKind::Collate { child, collation } => {
                let child = self.bind_expression(child)?;
                let child = if child.return_type.id() == LogicalTypeId::StringLiteral {
                    child.cast_to(&LogicalType::VARCHAR, false)
                } else {
                    child
                };
                if child.return_type.id() != LogicalTypeId::Varchar || child.return_type.alias().is_some() {
                    // `BindSelectNode`: of a position, the select list entry is no VARCHAR
                    if expr.alias == super::statements::ORDER_POSITION_COLLATE {
                        return Err(binder_error("COLLATE can only be applied to varchar columns"));
                    }
                    return Err(binder_error("collations are only supported for type varchar"));
                }
                crate::planner::collation::test_collation(collation)?;
                let mut child = child;
                child.return_type = LogicalType::varchar_collation(collation);
                child
            }
            ExprKind::Case { checks, else_expr } => self.bind_case(checks, else_expr)?,
            ExprKind::Between { input, lower, upper } => self.bind_between(input, lower, upper)?,
            ExprKind::Operator {
                op: op @ (OperatorType::In | OperatorType::NotIn),
                children,
            } => self.bind_in(children, *op == OperatorType::NotIn)?,
            ExprKind::Operator {
                op: OperatorType::Try,
                children,
            } => {
                // what a TRY catches at runtime does not fail its binding either
                let outer = std::mem::replace(&mut self.inside_try, true);
                let child = crate::function::binder::inside_try(|| self.bind_expression(&children[0]));
                self.inside_try = outer;
                let child = child?;
                if child.is_volatile() {
                    return Err(binder_error(
                        "TRY can not be used in combination with a volatile function",
                    ));
                }
                let ty = child.return_type.clone();
                BoundExpr::new(
                    BoundExprKind::Operator {
                        op: BoundOperator::Try,
                        children: vec![child],
                    },
                    ty,
                )
            }
            ExprKind::Operator {
                op: OperatorType::ArraySlice,
                children,
            } => {
                let bound = children
                    .iter()
                    .map(|c| self.bind_expression(c))
                    .collect::<Result<Vec<_>>>()?;
                // `BindOperatorAsFunction`: the name is looked up as a call would be, and a macro can't take
                // bound children; a system macro's body is bound in the system catalog
                let name = ast::QualifiedName {
                    qualification: Vec::new(),
                    name: "array_slice".to_string(),
                };
                if self.system_macro_depth == 0 && self.find_macro(&name, false)?.is_some() {
                    return Err(binder_error("array_slice is not a scalar function"));
                }
                bind_generic_function("array_slice", bound)?
            }
            ExprKind::Operator {
                op: OperatorType::ArrayConstructor,
                children,
            } => {
                let bound = children
                    .iter()
                    .map(|c| self.bind_expression(c))
                    .collect::<Result<Vec<_>>>()?;
                bind_nested_function("list_value", vec![String::new(); bound.len()], bound)?
            }
            ExprKind::Operator {
                op: op @ (OperatorType::ArrayExtract | OperatorType::StructExtract),
                children,
            } => {
                let bound = children
                    .iter()
                    .map(|c| self.bind_expression(c))
                    .collect::<Result<Vec<_>>>()?;
                // `BindOperatorExpression`: `.key` of a VARIANT is its variant_extract
                let variant = bound
                    .first()
                    .is_some_and(|b| b.return_type.id() == LogicalTypeId::Variant);
                // `BindOperatorExpression`: `[key]` of a JSON is its json_extract, of an array element (`$[i]`) or
                // of a field (`$."key"`)
                if *op == OperatorType::ArrayExtract
                    && bound.len() == 2
                    && crate::function::json::is_json_type(&bound[0].return_type)
                {
                    let mut bound = bound;
                    if let BoundExprKind::Constant(value) = &bound[1].kind
                        && !value.is_null()
                    {
                        let path = match crate::function::cast::cast_value(value, &LogicalType::UINTEGER) {
                            Ok(index) => Some(format!("$[{}]", index.as_i128().unwrap_or(0))),
                            Err(_) if value.logical_type().id() == LogicalTypeId::Varchar => {
                                Some(format!("$.\"{}\"", value.try_to_string()?))
                            }
                            Err(_) => None,
                        };
                        if let Some(path) = path {
                            bound[1] = BoundExpr::constant(Value::varchar(path));
                        }
                    }
                    let key = bound.pop().expect("two children");
                    let child = bound.pop().expect("two children");
                    return crate::function::json::bind_function("json_extract", vec![child, key], &|_| {
                        Ok(LogicalType::VARCHAR)
                    });
                }
                let name = if *op == OperatorType::StructExtract && !variant {
                    "struct_extract"
                } else {
                    "array_extract"
                };
                bind_nested_function(name, vec![String::new(); bound.len()], bound)?
            }
            ExprKind::Operator {
                op: OperatorType::Coalesce,
                children,
            } => {
                let (children, return_type) = self.resolve_coalesce_type(children, None)?;
                BoundExpr::new(
                    BoundExprKind::Operator {
                        op: BoundOperator::Coalesce,
                        children,
                    },
                    return_type,
                )
            }
            ExprKind::Conjunction { and, children } => {
                let children = children
                    .iter()
                    .map(|c| Ok(self.bind_expression(c)?.cast_to(&LogicalType::BOOLEAN, false)))
                    .collect::<Result<Vec<_>>>()?;
                BoundExpr::new(BoundExprKind::Conjunction { and: *and, children }, LogicalType::BOOLEAN)
            }
            // `GROUPING` of the groups of a query around this one is that query's (`BindGroupingFunction`)
            ExprKind::Operator {
                op: OperatorType::Grouping,
                children,
            } if self.aggregate_of_outer_query(children) => {
                self.pending_outer_aggregate = Some(expr.clone());
                return Err(crate::unported(OUTER_AGGREGATE));
            }
            ExprKind::Operator {
                op: OperatorType::Grouping,
                children,
            } => self.bind_grouping_function(children, expr.to_string())?,
            ExprKind::Operator { op, children } => {
                let op = match op {
                    OperatorType::Not => BoundOperator::Not,
                    OperatorType::IsNull => BoundOperator::IsNull,
                    OperatorType::IsNotNull => BoundOperator::IsNotNull,
                    other => return Err(crate::unported(format!("the {other:?} operator"))),
                };
                let mut children = children
                    .iter()
                    .map(|c| self.bind_expression(c))
                    .collect::<Result<Vec<_>>>()?;
                if op == BoundOperator::Not {
                    children = children
                        .into_iter()
                        .map(|c| c.cast_to(&LogicalType::BOOLEAN, false))
                        .collect();
                }
                BoundExpr::new(BoundExprKind::Operator { op, children }, LogicalType::BOOLEAN)
            }
            ExprKind::Cast {
                child,
                target,
                try_cast,
            } => {
                let target = self.bind_type(target)?;
                let child = self.bind_expression(child)?;
                if !crate::function::cast::has_cast(&child.return_type, &target) {
                    return Err(crate::unported(format!("casting {} to {target}", child.return_type)));
                }
                // a cast to the expression's own type is kept, so the result keeps its alias
                BoundExpr::new(
                    BoundExprKind::Cast {
                        child: Box::new(child),
                        try_cast: *try_cast,
                    },
                    target,
                )
            }
            ExprKind::Function(function)
                if builtin_qualification(&function.name.qualification)
                    && BuiltinFunctions::get().aggregate(&function.name.name).is_some() =>
            {
                // `BindAggregate`: the column of the aggregate has the name of its alias
                let bound = self.bind_aggregate(function)?;
                if expr.alias.is_empty() {
                    bound
                } else {
                    bound.with_alias(expr.alias.clone())
                }
            }
            ExprKind::Function(function) => {
                if !builtin_qualification(&function.name.qualification) {
                    // `ColumnQualifier::QualifyFunction`: a system function with a qualification that
                    // names no schema of it is a call on a column, "x.lower()" being "lower(x)"
                    if !is_system_function(&function.name.name) {
                        return Err(Error::new(
                            ErrorKind::Catalog,
                            format!("Scalar Function with name {} does not exist!", function.name.name),
                        ));
                    }
                    let column: Vec<String> = function
                        .name
                        .qualification
                        .iter()
                        .filter(|q| !q.is_empty())
                        .cloned()
                        .collect();
                    let mut call = function.clone();
                    call.name.qualification.clear();
                    call.args.insert(
                        0,
                        ast::FunctionArgument {
                            name: String::new(),
                            expr: Expr::from(ExprKind::ColumnRef(column)),
                        },
                    );
                    return self.bind_expression(&Expr::from(ExprKind::Function(call)));
                }
                if let Some(bound) = self.bind_session_function(function)? {
                    return Ok(bound);
                }
                if let Some(bound) = self.bind_lambda_function(function)? {
                    return Ok(bound);
                }
                if function.name.name.eq_ignore_ascii_case("nullif") {
                    let [left, right] = function.args.as_slice() else {
                        return Err(binder_error("nullif expects two arguments"));
                    };
                    let equal = Expr::comparison(ComparisonOp::Equal, left.expr.clone(), right.expr.clone());
                    return self.bind_case(&[(equal, Expr::constant(Literal::null()))], &left.expr);
                }
                // `BindBetweenFun`: the function BETWEEN binds to is internal
                if function.name.name.eq_ignore_ascii_case("__between") && function.args.len() == 3 {
                    for argument in &function.args {
                        self.bind_expression(&argument.expr)?;
                    }
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Between function cannot be called directly",
                    ));
                }
                if function.name.name.eq_ignore_ascii_case("if") {
                    let [condition, then, otherwise] = function.args.as_slice() else {
                        return Err(binder_error("if expects three arguments"));
                    };
                    return self.bind_case(&[(condition.expr.clone(), then.expr.clone())], &otherwise.expr);
                }
                if is_unnest(&function.name.name) && !self.lambda_scopes.is_empty() {
                    return Err(binder_error("UNNEST in lambda expressions is not supported"));
                }
                if is_unnest(&function.name.name) && function.args.len() > 1 {
                    // the levels the options ask for, as one UNNEST each
                    let (max_depth, _) = self.unnest_options(function)?;
                    let argument = &function.args[0].expr;
                    let ty = self.bind_expression(argument)?.return_type;
                    let (lists, structs) = super::statements::unnest_depths(&ty, max_depth);
                    if structs > 0 {
                        return Err(binder_error(
                            "UNNEST() on a struct column can only be applied as the root element of a SELECT \
                             expression",
                        ));
                    }
                    let mut nested = argument.clone();
                    for _ in 0..lists.max(1) {
                        let mut unnest = function.clone();
                        unnest.name.name = super::statements::UNNEST_LEVEL.into();
                        unnest.args = vec![duckrs_parser::ast::FunctionArgument::positional(nested)];
                        nested = Expr::from(ExprKind::Function(unnest));
                    }
                    nested.alias = expr.alias.clone();
                    return self.bind_expression(&nested);
                }
                let level = function.name.name == super::statements::UNNEST_LEVEL;
                if level || is_unnest(&function.name.name) {
                    if !level && self.unnest_level > 0 {
                        return Err(binder_error(
                            "Nested UNNEST calls are not supported - use UNNEST(x, recursive := true) to unnest \
                             multiple levels",
                        ));
                    }
                    if !level {
                        self.unnest_level += 1;
                    }
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>();
                    if !level {
                        self.unnest_level -= 1;
                    }
                    return bind_unnest(children?);
                }
                if is_generic_function(&function.name.name) {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    // `BindMinMax` under `default_collation`: the strings compare collated
                    let lower = function.name.name.to_ascii_lowercase();
                    let default = self.default_collation();
                    let aggregate_name = |at: usize| match children.get(at).map(|c| &c.kind) {
                        Some(BoundExprKind::Constant(value)) => value.try_to_string().ok(),
                        _ => None,
                    };
                    let min_max = matches!(lower.as_str(), "list_max" | "list_min" | "array_max" | "array_min")
                        || matches!(
                            lower.as_str(),
                            "list_aggr" | "list_aggregate" | "array_aggr" | "array_aggregate" | "aggregate"
                        ) && children.len() == 2
                            && aggregate_name(1)
                                .is_some_and(|n| n.eq_ignore_ascii_case("min") || n.eq_ignore_ascii_case("max"));
                    if min_max
                        && !default.is_empty()
                        && let Some(list) = children.first()
                        && list.return_type.id() == LogicalTypeId::List
                        && list
                            .return_type
                            .child_type()
                            .is_some_and(|c| c.id() == LogicalTypeId::Varchar && c.collation().is_none())
                    {
                        // the aggregate's name carries the collation to `list_aggr`
                        let collated = LogicalType::VARCHAR.with_collation(Some(&default));
                        let name = aggregate_name(1).unwrap_or_else(|| lower[lower.len() - 3..].to_string());
                        let list = list.clone();
                        drop(children);
                        let which = BoundExpr::constant(Value::varchar(name).reinterpret(&collated)?);
                        // `arg_max` returns the item, which has no collation
                        let mut bound = bind_generic_function("list_aggr", vec![list, which])?;
                        bound.return_type = bound.return_type.with_collation(None);
                        return Ok(bound);
                    }
                    return bind_generic_function(&function.name.name, children);
                }
                if ["st_crs", "st_setcrs"]
                    .iter()
                    .any(|name| function.name.name.eq_ignore_ascii_case(name))
                {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    return if function.name.name.eq_ignore_ascii_case("st_crs") {
                        crate::function::geometry::bind_st_crs(children)
                    } else {
                        crate::function::geometry::bind_st_setcrs(children)
                    };
                }
                if function.name.name.eq_ignore_ascii_case("vertex_extract") {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    return crate::function::geometry::bind_vertex_extract_call(children);
                }
                if is_variant_function(&function.name.name) {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    return bind_variant_function(&function.name.name, children);
                }
                if matches!(
                    function.name.name.to_ascii_lowercase().as_str(),
                    "list_reverse_sort" | "array_reverse_sort"
                ) {
                    let mut call = function.clone();
                    call.name.name = "list_sort".to_string();
                    call.args.insert(
                        1,
                        ast::FunctionArgument {
                            name: String::new(),
                            expr: Expr::constant(ast::Literal::string("DESC".to_string())),
                        },
                    );
                    return self.bind_expression(&Expr::from(ExprKind::Function(call)));
                }
                if function.name.name.eq_ignore_ascii_case("write_log") && function.name.qualification.is_empty() {
                    return self.bind_write_log(function);
                }
                if function.name.name.eq_ignore_ascii_case("parse_duckdb_log_message")
                    && function.name.qualification.is_empty()
                {
                    return self.bind_parse_log_message(function);
                }
                // Ported from duckdb/extension/core_functions/scalar/generic/type_functions.cpp @ rust-port-baseline
                // `get_type(x)`: the type of `x`, as a TYPE
                if function.name.name.eq_ignore_ascii_case("get_type")
                    && function.name.qualification.is_empty()
                    && function.args.len() == 1
                {
                    let child = self.bind_expression(&function.args[0].expr)?;
                    if child.return_type.id() == LogicalTypeId::Unknown {
                        return Err(Error::new(
                            ErrorKind::ParameterNotResolved,
                            "Parameter types could not be resolved",
                        ));
                    }
                    return Ok(BoundExpr::constant(type_value(&child.return_type)));
                }
                if function.name.name.eq_ignore_ascii_case("invoke")
                    && function.name.qualification.is_empty()
                    && function
                        .args
                        .first()
                        .is_some_and(|a| !matches!(a.expr.kind, ExprKind::Lambda { .. }))
                {
                    return Err(binder_error("Invalid lambda expression passed to 'invoke' function."));
                }
                // `invoke(lambda x, y: body, a, b)`: the body with the arguments for its parameters
                if function.name.name.eq_ignore_ascii_case("invoke")
                    && function.name.qualification.is_empty()
                    && let Some(first) = function.args.first()
                    && let ExprKind::Lambda { lhs, expr: body, .. } = &first.expr.kind
                {
                    fn names(lhs: &Expr, out: &mut Vec<String>) {
                        match &lhs.kind {
                            ExprKind::ColumnRef(n) => out.push(n[0].clone()),
                            ExprKind::Function(f) => f.args.iter().for_each(|a| names(&a.expr, out)),
                            ExprKind::Operator { children, .. } => children.iter().for_each(|c| names(c, out)),
                            _ => {}
                        }
                    }
                    let mut parameters = Vec::new();
                    names(lhs, &mut parameters);
                    // an argument is known by its parameter's name in the body (what `alias(x)` gives)
                    let arguments: Vec<Expr> = function.args[1..]
                        .iter()
                        .zip(&parameters)
                        .map(|(a, name)| a.expr.clone().with_alias(name.clone()))
                        .collect();
                    if parameters.len() != function.args.len() - 1 {
                        return Err(binder_error(format!(
                            "The number of lambda parameters does not match the number of arguments passed to the \
                             'invoke' function, expected {}{}, got {}.",
                            if parameters.len() > function.args.len() - 1 {
                                "at least "
                            } else {
                                ""
                            },
                            parameters.len(),
                            function.args.len() - 1
                        )));
                    }
                    let mut body = (**body).clone();
                    super::macros::replace_in_expr(&mut body, &parameters, &arguments);
                    body.alias = expr.alias.clone();
                    return self.bind_expression(&body);
                }
                // `make_type(name, ...)`: the type the name and its parameters make, as a TYPE
                if function.name.name.eq_ignore_ascii_case("make_type") && function.name.qualification.is_empty() {
                    return self.bind_make_type(function);
                }
                if function.name.name.eq_ignore_ascii_case("to_aggregate_state") {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    let resolve = |name: &str| self.bind_named_type(&LogicalType::unbound(name.to_string()));
                    return crate::function::aggregates::bind_to_aggregate_state(children, &resolve);
                }
                if crate::function::extras::is_extra_function(&function.name.name) {
                    let names: Vec<String> = function.args.iter().map(|a| a.name.clone()).collect();
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    // a parameter's type is only known when the statement runs (`ParameterNotResolvedException`)
                    let preparing = self.parameters.as_ref().is_some_and(|p| p.values.is_none());
                    let typed_by_argument = matches!(
                        function.name.name.to_lowercase().as_str(),
                        "cast_to_type" | "replace_type" | "struct_concat" | "struct_insert" | "struct_update"
                    );
                    if preparing
                        && typed_by_argument
                        && children.iter().any(|c| c.return_type.id() == LogicalTypeId::SqlNull)
                    {
                        return Ok(BoundExpr::constant(Value::sql_null()));
                    }
                    let mut bound = crate::function::extras::bind_function(&function.name.name, &names, children)?;
                    // `BindIEEEFloatingUnary` of the functions bound here
                    if self.options.ieee_floating_point_ops
                        && let BoundExprKind::Function {
                            function: bound_function,
                            children,
                            ..
                        } = &mut bound.kind
                        && let Some(callback) =
                            crate::function::math::ieee_variant(&bound_function.name, children.len())
                    {
                        *bound_function = Arc::new(crate::function::ScalarFunction::new(
                            bound_function.name.clone(),
                            bound_function.arguments.clone(),
                            bound_function.return_type.clone(),
                            callback,
                        ));
                    }
                    // `BindBinaryFloatingPoint` of fmod and fdiv: a zero divisor is an error without IEEE semantics,
                    // or NULL with error_on_division_by_zero off
                    if matches!(function.name.name.to_lowercase().as_str(), "fmod" | "fdiv")
                        && !self.options.ieee_floating_point_ops
                        && let BoundExprKind::Function { function, .. } = &mut bound.kind
                    {
                        *function = Arc::new(function.with_zero_check(!self.options.error_on_division_by_zero));
                    }
                    return Ok(bound);
                }
                if crate::function::json_serialize::is_serialize_function(&function.name.name) {
                    let names: Vec<String> = function.args.iter().map(|a| a.name.clone()).collect();
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    let target = self.storage_compatibility();
                    return crate::function::json_serialize::bind_function(
                        &function.name.name,
                        &names,
                        children,
                        target,
                    );
                }
                if crate::function::json::is_json_function(&function.name.name) {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    let types = std::cell::RefCell::new(self.child_binder());
                    return crate::function::json::bind_function(&function.name.name, children, &|name| {
                        types.borrow_mut().csv_type(name)
                    });
                }
                if is_nested_function(&function.name.name) {
                    let names = function.args.iter().map(|a| a.name.clone()).collect();
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    // a parameter as the key: its value is only known when the statement runs
                    // (`ParameterNotResolvedException`)
                    let preparing = self.parameters.as_ref().is_some_and(|p| p.values.is_none());
                    if preparing
                        && function.name.name.eq_ignore_ascii_case("union_extract")
                        && function
                            .args
                            .get(1)
                            .is_some_and(|a| matches!(a.expr.kind, ExprKind::Parameter(_)))
                    {
                        return Ok(BoundExpr::constant(Value::sql_null()));
                    }
                    // an ENUM parameter: its type is only known when the statement runs
                    if preparing
                        && ["enum_code", "enum_first", "enum_last", "enum_range"]
                            .iter()
                            .any(|n| function.name.name.eq_ignore_ascii_case(n))
                        && function
                            .args
                            .first()
                            .is_some_and(|a| matches!(a.expr.kind, ExprKind::Parameter(_)))
                    {
                        return Ok(BoundExpr::constant(Value::sql_null()));
                    }
                    return bind_nested_function(&function.name.name, names, children);
                }
                // `AliasFun`: the name of its argument
                if function.name.name.eq_ignore_ascii_case("alias")
                    && function.name.qualification.iter().all(String::is_empty)
                    && let [argument] = function.args.as_slice()
                    && argument.name.is_empty()
                {
                    let bound = self.bind_expression(&argument.expr)?;
                    // `AliasFunction`: the call's own alias, if it has one
                    let name = if !expr.alias.is_empty() {
                        expr.alias.clone()
                    } else if argument.expr.alias.is_empty() {
                        // `Expression::GetName`: a bound column is named by its column, unqualified
                        let mut unqualified = argument.expr.clone();
                        visit_expr_mut(&mut unqualified, &mut |e: &mut Expr| {
                            if let ExprKind::ColumnRef(names) = &mut e.kind
                                && names.len() > 1
                            {
                                *names = vec![names.last().expect("a name").clone()];
                            }
                        });
                        super::statements::expression_name(&unqualified)
                    } else {
                        argument.expr.alias.clone()
                    };
                    // the argument is evaluated all the same: its errors surface (a constant one folds)
                    if bound.is_foldable() && crate::execution::expression_executor::evaluate_scalar(&bound).is_ok() {
                        return Ok(BoundExpr::constant(Value::varchar(name)));
                    }
                    let name = BoundExpr::constant(Value::varchar(name));
                    return Ok(crate::function::nested::bound_function(
                        "alias",
                        vec![bound, name],
                        LogicalType::VARCHAR,
                        alias_function,
                    ));
                }
                // the macros of the system catalog (`DefaultFunctionGenerator`), where no function has
                // the name
                if BuiltinFunctions::get().scalar(&function.name.name).is_none() {
                    let schema = function
                        .name
                        .qualification
                        .iter()
                        .rfind(|q| !q.is_empty())
                        .map(String::as_str);
                    if let Some(entry) = crate::catalog::defaults::default_macro(schema, &function.name.name, false) {
                        return self.bind_macro(function, &entry);
                    }
                }
                if function.args.iter().any(|a| !a.name.is_empty()) {
                    return Err(crate::unported("named function arguments"));
                }
                // what the parser makes of `RUNNING f(x)`, which MATCH_RECOGNIZE takes away before binding
                if function.name.qualification.is_empty()
                    && matches!(function.name.name.as_str(), "__mr_running" | "__mr_final")
                {
                    return Err(binder_error(
                        "RUNNING and FINAL are only meaningful in the MEASURES of a MATCH_RECOGNIZE",
                    ));
                }
                let Some(set) = BuiltinFunctions::get().scalar(&function.name.name) else {
                    // `ExpressionBinder::BindFunction`: a table function, used as a scalar one
                    const TABLE_FUNCTIONS: &[&str] = &[
                        "read_csv",
                        "read_csv_auto",
                        "sniff_csv",
                        "read_parquet",
                        "parquet_scan",
                        "parquet_metadata",
                        "parquet_schema",
                        "read_json",
                        "read_json_auto",
                        "read_ndjson",
                        "read_ndjson_auto",
                        "read_json_objects",
                        "read_text",
                        "read_blob",
                        "glob",
                        "query",
                        "query_table",
                        "summary",
                        "repeat_row",
                        "test_all_types",
                        "test_vector_types",
                        "pragma_table_info",
                        "pragma_show",
                        "pragma_storage_info",
                        "pragma_metadata_info",
                        "duckdb_settings",
                        "duckdb_extensions",
                        "duckdb_temporary_files",
                        "duckdb_memory",
                        "duckdb_logs",
                        "duckdb_functions",
                        "duckdb_keywords",
                        "duckdb_types",
                        "duckdb_sequences",
                        "duckdb_databases",
                        "duckdb_schemas",
                        "duckdb_tables",
                        "duckdb_views",
                        "duckdb_columns",
                        "duckdb_indexes",
                        "duckdb_constraints",
                        "duckdb_dependencies",
                        "duckdb_optimizers",
                        "pragma_version",
                        "pragma_platform",
                        "pragma_user_agent",
                        "pragma_database_size",
                    ];
                    // `ExpressionBinder::BindWindow` of a window function written without OVER
                    if function.name.qualification.iter().all(String::is_empty)
                        && super::window::window_function(&function.name.name.to_lowercase()).is_some()
                    {
                        return Err(binder_error("Window functions are not supported here"));
                    }
                    if function.name.qualification.iter().all(String::is_empty)
                        && TABLE_FUNCTIONS.contains(&function.name.name.to_lowercase().as_str())
                    {
                        return Err(binder_error(format!(
                            "Function {} is a table function but it was used as a scalar function. This function \
                             has to be called in a FROM clause (similar to a table).",
                            function.name.name
                        )));
                    }
                    // `CatalogException::MissingEntry`: the most similar function of the catalog
                    let functions = BuiltinFunctions::get();
                    let mut best: Option<(f64, String)> = None;
                    let names = functions
                        .scalar_sets()
                        .into_iter()
                        .map(|set| set.name.clone())
                        .chain(functions.aggregate_sets().into_iter().map(|set| set.name.clone()))
                        .chain(
                            crate::function::extras::STRING_FUNCTIONS
                                .iter()
                                .chain(crate::function::extras::OTHER_FUNCTIONS.iter())
                                .map(|name| name.to_string()),
                        );
                    // the functions bound by name, and the macros of the system catalog
                    const BOUND_BY_NAME: &[&str] = &["nullif", "if", "write_log", "current_setting", "getvariable"];
                    let names = names.chain(BOUND_BY_NAME.iter().map(|name| name.to_string())).chain(
                        crate::catalog::defaults::default_macro_names()
                            .filter(|(_, _, table_macro)| !table_macro)
                            .map(|(_, name, _)| name.to_string()),
                    );
                    for name in names {
                        let score = duckrs_common::string_util::similarity_rating(&name, &function.name.name);
                        if score > best.as_ref().map_or(0.0, |(b, _)| *b) {
                            best = Some((score, name));
                        }
                    }
                    // `SimilarEntriesInSchemas`: each schema's best, ties kept
                    let mut found: Vec<(f64, String)> = best.into_iter().collect();
                    for schema in self.catalog.schemas() {
                        let mut schema_best: Option<(f64, String)> = None;
                        for entry in schema.macro_entries(false) {
                            let score = duckrs_common::string_util::similarity_rating(&entry.name, &function.name.name);
                            if score > schema_best.as_ref().map_or(0.0, |(b, _)| *b) {
                                schema_best = Some((score, entry.name.clone()));
                            }
                        }
                        let Some((score, name)) = schema_best else { continue };
                        match found.first().map(|(b, _)| *b) {
                            Some(b) if score < b => {}
                            Some(b) if score > b => found = vec![(score, name)],
                            _ if found.iter().any(|(_, n)| n == &name) => {}
                            _ => found.push((score, name)),
                        }
                    }
                    // `CMUtils::Bind`: they are in the catalog in C++, for (de)serialization only
                    let lower = function.name.name.to_ascii_lowercase();
                    if lower.starts_with("__internal_compress_") || lower.starts_with("__internal_decompress_") {
                        return Err(binder_error(
                            "Compressed materialization functions are for internal use only!".to_string(),
                        ));
                    }
                    let mut suggestions: Vec<String> = found.into_iter().map(|(_, name)| name).collect();
                    suggestions.sort();
                    let hint = if suggestions.is_empty() {
                        String::new()
                    } else {
                        format!("\nDid you mean \"{}\"?", suggestions.join(" or "))
                    };
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("Scalar Function with name {} does not exist!{hint}", function.name.name),
                    ));
                };
                if function.filter.is_some() || !function.order_by.is_empty() || function.distinct {
                    return Err(crate::unported("aggregate modifiers on scalar functions"));
                }
                let children = function
                    .args
                    .iter()
                    .map(|a| self.bind_expression(&a.expr))
                    .collect::<Result<Vec<_>>>()?;
                match (set.name.as_str(), children.as_slice()) {
                    ("typeof", [child]) if child.return_type.id() != LogicalTypeId::Unknown => {
                        BoundExpr::constant(typeof_constant(&child.return_type))
                    }
                    ("strftime" | "strptime" | "try_strptime", [first, second])
                        if {
                            // strftime takes its arguments in either order
                            let swapped = set.name == "strftime" && first.return_type.id() == LogicalTypeId::Varchar;
                            let format = if swapped { first } else { second };
                            !format.is_foldable() && format.return_type.id() != LogicalTypeId::Unknown
                        } =>
                    {
                        // `BindScalarFunctionInput::GetConstant` of the named "format" parameter
                        return Err(binder_error(format!(
                            "The format argument in function {} must be a constant expression",
                            set.name
                        )));
                    }
                    // `StrpTimeFunction::Bind`: the format decides the type of the result
                    ("strptime" | "try_strptime", [_, format]) if format.is_foldable() => {
                        let ty = match try_evaluate_scalar(format) {
                            Some(value) => crate::function::dates::strptime_return_type(&value)?,
                            None => None,
                        };
                        let mut call =
                            bind_scalar_function(set, children, function.is_operator, &|e| try_evaluate_scalar(e))?;
                        if let Some(ty) = ty {
                            call.return_type = ty;
                        }
                        call
                    }
                    // `RegexpExtractAllStruct` / `RegexExtractBind`: a list of group names makes a STRUCT
                    ("regexp_extract" | "regexp_extract_all", [_, _, groups, ..])
                        if children.len() <= 4 && groups.return_type.id() == LogicalTypeId::List =>
                    {
                        crate::function::regexp::bind_regexp_struct(&set.name, children)?
                    }
                    // a string literal group fits `(.., INTEGER, VARCHAR)` as well as `(.., VARCHAR[], VARCHAR)`
                    ("regexp_extract", [_, _, _, _])
                        if matches!(
                            &function.args[2].expr.kind,
                            ExprKind::Constant(literal) if literal.kind == LiteralKind::String
                        ) =>
                    {
                        let types: Vec<String> = children
                            .iter()
                            .map(|c| crate::function::binder::expression_return_type(c).to_string())
                            .collect();
                        return Err(binder_error(format!(
                            "Could not choose a best candidate function for the function call \
                             \"regexp_extract({})\". In order to select one, please add explicit type casts.\n\
                             \tCandidate functions:\n\tregexp_extract(VARCHAR, VARCHAR, INTEGER, VARCHAR) -> \
                             VARCHAR\n\tregexp_extract(VARCHAR, VARCHAR, VARCHAR[], VARCHAR) -> STRUCT\n",
                            types.join(", ")
                        )));
                    }
                    // `RegexExtractBind`: the third argument is a constant, the group or the options
                    ("regexp_extract", [_, _, group]) if !group.is_foldable() => {
                        return Err(binder_error(
                            "Argument #3 in function regexp_extract must be a constant expression",
                        ));
                    }
                    ("regexp_extract", [_, _, options])
                        if matches!(
                            options.return_type.id(),
                            LogicalTypeId::Varchar | LogicalTypeId::StringLiteral
                        ) =>
                    {
                        let mut children = children;
                        let options = children.pop().expect("three arguments");
                        children.push(BoundExpr::constant(Value::integer(0)));
                        children.push(options);
                        bind_scalar_function(set, children, function.is_operator, &|e| try_evaluate_scalar(e))?
                    }
                    // `OperatorAddFun`: list_concat is an overload of `+`
                    ("+", [left, right])
                        if [left, right]
                            .iter()
                            .all(|c| matches!(c.return_type.id(), LogicalTypeId::List | LogicalTypeId::Array)) =>
                    {
                        crate::function::generic::bind_generic_function("list_concat", children)?
                    }
                    (name, _) => match bind_decimal_round(name, &children)? {
                        Some(bound) => bound,
                        None => {
                            let division = matches!(set.name.as_str(), "/" | "//" | "%" | "mod" | "divide" | "fmod");
                            let checked = division
                                && (!self.options.error_on_division_by_zero || !self.options.ieee_floating_point_ops);
                            let fold = |e: &BoundExpr| if checked { None } else { try_evaluate_scalar(e) };
                            let mut bound = bind_scalar_function(set, children, function.is_operator, &fold)?;
                            // `BindDivisionByZero` / `BindBinaryFloatingPoint`: a zero divisor gives NULL with
                            // error_on_division_by_zero off; IEEE floats give inf and NaN, else what integers give
                            // `BindIEEEFloatingUnary`: the IEEE function, where it has one
                            if self.options.ieee_floating_point_ops
                                && let BoundExprKind::Function { function, children, .. } = &mut bound.kind
                                && function.return_type.id() == LogicalTypeId::Double
                                && let Some(callback) = crate::function::math::ieee_variant(&set.name, children.len())
                            {
                                *function = Arc::new(crate::function::ScalarFunction::new(
                                    function.name.clone(),
                                    function.arguments.clone(),
                                    function.return_type.clone(),
                                    callback,
                                ));
                            }
                            // `BindIntervalDivide`: an INTERVAL divided by zero, whatever the IEEE setting
                            if set.name == "/"
                                && let BoundExprKind::Function { function, .. } = &mut bound.kind
                                && function.return_type.id() == LogicalTypeId::Interval
                            {
                                *function = Arc::new(function.with_zero_check(!self.options.error_on_division_by_zero));
                            } else if checked && let BoundExprKind::Function { function, .. } = &mut bound.kind {
                                // (`//` of floats is `BindDivisionByZero`, as for integers)
                                let floating = set.name != "//"
                                    && matches!(
                                        function.return_type.id(),
                                        LogicalTypeId::Float | LogicalTypeId::Double
                                    );
                                let null = !self.options.error_on_division_by_zero;
                                if !floating && null {
                                    *function = Arc::new(function.with_zero_check(true));
                                } else if floating && !self.options.ieee_floating_point_ops {
                                    *function = Arc::new(function.with_zero_check(null));
                                }
                            }
                            self.bind_sequence_function(bound)?
                        }
                    },
                }
            }
            ExprKind::Subquery { .. } if !self.lambda_scopes.is_empty() => {
                return Err(binder_error("subqueries in lambda expressions are not supported"));
            }
            ExprKind::Subquery {
                subquery,
                subquery_type,
                child,
                comparison,
            } => self.bind_subquery(subquery, *subquery_type, child.as_deref(), *comparison)?,
            ExprKind::Parameter(identifier) => {
                // `Binder::Bind(ParameterExpression)`
                let key = identifier.to_lowercase();
                let preparing = self.parameters.as_ref().is_some_and(|p| p.values.is_none());
                let given = self
                    .parameters
                    .as_ref()
                    .and_then(|p| p.values.as_ref())
                    .is_some_and(|values| values.contains_key(&key));
                // no value given: a user variable of that name, unless it is a position
                if !preparing
                    && !given
                    && !key.starts_with(|c: char| c.is_ascii_digit())
                    && let Some(value) = self.session.as_ref().and_then(|s| s.variables.get(&key))
                {
                    return Ok(BoundExpr::constant(value.clone()));
                }
                let Some(parameters) = &self.parameters else {
                    // a statement run directly has no values for its parameters
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("Values were not provided for the following parameters: {identifier}"),
                    ));
                };
                let mut seen = parameters.seen.lock();
                if !seen.contains(&key) {
                    seen.push(key.clone());
                }
                let value = parameters.values.as_ref().and_then(|values| values.get(&key));
                let constant = BoundExpr::constant(value.cloned().unwrap_or_else(Value::sql_null));
                if parameters.typed.contains(&key) {
                    // not a literal (`GetExpressionReturnType` sees a constant as one): of the type it has
                    let return_type = constant.return_type.clone();
                    BoundExpr {
                        kind: BoundExprKind::Cast {
                            child: Box::new(constant),
                            try_cast: false,
                        },
                        return_type,
                        alias: String::new(),
                    }
                } else {
                    constant
                }
            }
            ExprKind::Star(_) => return Err(binder_error("STAR expression is not supported here")),
            ExprKind::Default => {
                // `WhereBinder`, `GroupBinder` and `TableFunctionBinder` name their clause
                let clause = ["WHERE clause", "GROUP BY clause", "Table function", "DEFAULT value"]
                    .into_iter()
                    .find(|clause| self.aggregate_error == format!("{clause} cannot contain aggregates!"));
                return Err(binder_error(match clause {
                    Some(clause) => format!("{clause} cannot contain DEFAULT clause"),
                    None => "DEFAULT is not allowed here!".to_string(),
                }));
            }
            ExprKind::Window(window) => return self.bind_window(window),
            other => {
                let name = format!("{other:?}");
                let kind: String = name.chars().take_while(|c| c.is_alphanumeric()).collect();
                return Err(crate::unported(format!("{kind} expressions")));
            }
        };
        Ok(bound)
    }
}

/// Whether a function's qualification can name a built-in function, which live in the `main`
/// schema of the system catalog.
/// Whether `name` is a function of the system catalog: one of the built-ins, however duckrs binds
/// it, or a system macro.
pub(super) fn is_system_function(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let functions = BuiltinFunctions::get();
    functions.scalar(name).is_some()
        || functions.aggregate(name).is_some()
        || crate::catalog::defaults::default_macro(None, name, false).is_some()
        || super::lambda::is_lambda_function(name)
        || is_generic_function(name)
        || is_unnest(name)
        || is_variant_function(name)
        || crate::function::extras::is_extra_function(name)
        || crate::function::json::is_json_function(name)
        || is_nested_function(name)
        || lower.starts_with("json_group_")
        || matches!(
            lower.as_str(),
            "nullif"
                | "if"
                | "list_reverse_sort"
                | "array_reverse_sort"
                | "write_log"
                | "parse_duckdb_log_message"
                | "to_aggregate_state"
                | "current_database"
                | "current_schema"
                | "current_schemas"
                | "in_search_path"
                | "current_query"
                | "txid_current"
                | "current_transaction_id"
                | "current_connection_id"
                | "current_query_id"
                | "getvariable"
                | "current_setting"
        )
}

/// `BindSchemaOrCatalog` and `GetCatalogEntry` of an `AGGREGATE_FUNCTION_ENTRY`: the built-in aggregate a
/// qualified name names, if any. A lone qualification that is a catalog is the catalog's `main` schema.
pub(super) fn qualified_aggregate(
    name: &ast::QualifiedName,
) -> Option<&'static crate::function::aggregates::AggregateFunctionSet> {
    let parts: Vec<&String> = name.qualification.iter().filter(|q| !q.is_empty()).collect();
    let catalog = |part: &String| part.eq_ignore_ascii_case("system") || part.eq_ignore_ascii_case(DEFAULT_CATALOG);
    let builtin = match parts.as_slice() {
        [only] => catalog(only) || builtin_qualification(&name.qualification),
        _ => builtin_qualification(&name.qualification),
    };
    if !builtin {
        return None;
    }
    BuiltinFunctions::get().aggregate(&name.name)
}

fn builtin_qualification(qualification: &[String]) -> bool {
    // an operator's qualification has an empty entry for what it does not name
    let parts: Vec<&String> = qualification.iter().filter(|q| !q.is_empty()).collect();
    let builtin_schema =
        |schema: &String| schema.eq_ignore_ascii_case("main") || schema.eq_ignore_ascii_case("pg_catalog");
    match parts.as_slice() {
        [] => true,
        [schema] => builtin_schema(schema),
        [catalog, schema] => {
            (catalog.eq_ignore_ascii_case("system") || catalog.eq_ignore_ascii_case(DEFAULT_CATALOG))
                && builtin_schema(schema)
        }
        _ => false,
    }
}

/// `QualifiedName::TryParse`: the dot-separated parts of a name, each either fully double-quoted
/// (a doubled quote is one quote) or not quoted at all.
pub(super) fn parse_qualified_name(input: &str) -> Result<Vec<String>> {
    let error = |message: &str| Err(Error::new(ErrorKind::Parser, format!("{message} (input: {input})")));
    let bytes = input.as_bytes();
    let mut parts = Vec::new();
    let mut idx = 0;
    // `TryParseComponents`
    while idx < bytes.len() {
        let mut entry = Vec::new();
        if bytes[idx] == b'"' {
            // `StringUtil::TryParseQuotedString`
            let mut closed = None;
            let mut i = idx + 1;
            while i < bytes.len() {
                if bytes[i] != b'"' {
                    entry.push(bytes[i]);
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    entry.push(b'"');
                    i += 1;
                } else {
                    closed = Some(i + 1);
                    break;
                }
                i += 1;
            }
            let Some(next) = closed else {
                return error("Unterminated quote in qualified name!");
            };
            idx = next;
            if entry.is_empty() {
                return error("Zero-length delimited identifier in qualified name!");
            }
            if idx < bytes.len() && bytes[idx] != b'.' {
                return error("Unexpected character after a quoted identifier in a qualified name!");
            }
        } else {
            while idx < bytes.len() && bytes[idx] != b'.' {
                if bytes[idx] == b'"' {
                    return error("Unexpected quote in the middle of a qualified name component!");
                }
                entry.push(bytes[idx]);
                idx += 1;
            }
        }
        parts.push(String::from_utf8_lossy(&entry).into_owned());
        if idx < bytes.len() {
            idx += 1;
        }
    }
    if parts.is_empty() || parts.iter().any(String::is_empty) {
        return error("Zero-length identifier in qualified name!");
    }
    Ok(parts)
}

/// `MetaTransaction::ModifyDatabase` of the database of a sequence that `nextval` or `setval` writes.
fn sequence_write_conflict(database: &str, written: &str) -> Error {
    Error::new(
        ErrorKind::Transaction,
        format!(
            "Attempting to write to database {database} in a transaction that has already modified database \
             {written} - a single transaction can only write to a single attached database."
        ),
    )
}

impl Binder<'_> {
    /// `NextValBind`: the name of a sequence function is a constant, looked up once.
    fn bind_sequence_function(&self, bound: BoundExpr) -> Result<BoundExpr> {
        let BoundExprKind::Function { function, children, .. } = &bound.kind else {
            return Ok(bound);
        };
        let op = match function.name.as_str() {
            "nextval" => SequenceOp::Next,
            "currval" => SequenceOp::Current,
            "setval" => SequenceOp::Set,
            _ => return Ok(bound),
        };
        let name = &children[0];
        if !name.is_foldable() {
            return Err(binder_error(format!(
                "The sequence_name argument in function {} must be a constant expression",
                function.name
            )));
        }
        let value = evaluate_scalar(name)?;
        if value.is_null() {
            return Ok(BoundExpr::constant(Value::null(LogicalType::BIGINT)));
        }
        let parts = parse_qualified_name(&value.try_to_string()?)?;
        let Some((name, qualification)) = parts.split_last() else {
            return Err(crate::unported("this sequence name"));
        };
        let qualified = ast::QualifiedName {
            qualification: qualification.to_vec(),
            name: name.clone(),
        };
        let (sequence, database) = self.lookup_sequence(&qualified)?;
        // `NextValModifiedDatabases`: nextval and setval write the database of the sequence
        if op != SequenceOp::Current
            && let Some(database) = database
            && let Some(session) = &self.session
        {
            let mut recorded = session.sequence_writes.lock().expect("not poisoned");
            match session.written_database.clone().or_else(|| recorded.clone()) {
                Some(written) if !written.eq_ignore_ascii_case(&database) => {
                    return Err(sequence_write_conflict(&database, &written));
                }
                Some(_) => {}
                None => *recorded = Some(database),
            }
        }
        Ok(BoundExpr::new(
            BoundExprKind::Sequence {
                op,
                sequence,
                arguments: children[1..].to_vec(),
            },
            LogicalType::BIGINT,
        ))
    }

    /// `BindSequence`: the sequence `name` names along the search path, and the database it is in (`None`
    /// for a temporary one): this binder's, another attached one that qualifies it, or the default one.
    fn lookup_sequence(&self, name: &ast::QualifiedName) -> Result<(Arc<SequenceCatalogEntry>, Option<String>)> {
        let session = self.session.as_ref();
        if let Some((catalog, schema)) = self.other_database_of(name) {
            let sequence = catalog.get_sequence(schema.as_deref(), None, &name.name)?;
            return Ok((sequence, name.qualification.first().cloned()));
        }
        let mut schema = self.schema_of(name)?;
        // a stored DEFAULT bound as the file loads, before it has the name it is attached by: what
        // qualifies the sequence beyond a schema of the file is its database (C++ binds it later, by name)
        if session.is_none()
            && let Some(first) = schema.as_deref().filter(|s| self.catalog.get_schema(s).is_none())
        {
            schema = match name.qualification.as_slice() {
                [_] => None,
                [_, inner] if self.catalog.get_schema(inner).is_some() => Some(inner.clone()),
                _ => Some(first.to_string()),
            };
        }
        let temporary = |sequence: &Arc<SequenceCatalogEntry>| {
            self.catalog
                .get_schema(crate::catalog::TEMP_SCHEMA)
                .and_then(|temp| temp.get_sequence(&sequence.name))
                .is_some_and(|temp| Arc::ptr_eq(&temp, sequence))
        };
        match self
            .catalog
            .get_sequence(schema.as_deref(), self.search_schema.as_deref(), &name.name)
        {
            Ok(sequence) => {
                let database = (!temporary(&sequence))
                    .then(|| session.map(|s| s.database.clone()))
                    .flatten();
                Ok((sequence, database))
            }
            Err(error) => {
                // past the search path of this database comes that of the connection's default one
                let fallback = session
                    .filter(|_| name.qualification.is_empty())
                    .filter(|s| !s.default_database.eq_ignore_ascii_case(&s.database))
                    .and_then(|s| Some(((s.attached_catalog)(&s.default_database)?, s.default_database.clone())))
                    .and_then(|(catalog, database)| {
                        Some((catalog.get_sequence(None, None, &name.name).ok()?, Some(database)))
                    });
                fallback.ok_or(error)
            }
        }
    }

    /// `BaseSelectBinder::TryBindGroup` + `BindGroup`: a reference to the group `expr` matches.
    pub(super) fn try_bind_group(&self, expr: &Expr) -> Option<BoundExpr> {
        let group = self.find_group(expr)?;
        let state = self.aggregates.as_ref()?;
        // `BindGroup` names the reference after the expression (`GetName`)
        let name = super::statements::expression_name(expr);
        if let Some((_, aggregate)) = state.collated_groups.iter().find(|(g, _)| *g == group) {
            let return_type = state.aggregates[*aggregate].return_type.clone();
            let first = BoundExpr::column_ref(
                ColumnBinding::new(state.aggregate_index, *aggregate),
                return_type.clone(),
                name.clone(),
            );
            if state.grouping_set_count <= 1 {
                return Some(first);
            }
            // with grouping sets, NULL where the set does not have the group
            let collated = BoundExpr::column_ref(
                ColumnBinding::new(state.group_index, group),
                state.groups[group].return_type.clone(),
                name.clone(),
            );
            let is_null = BoundExpr::new(
                BoundExprKind::Operator {
                    op: BoundOperator::IsNull,
                    children: vec![collated],
                },
                LogicalType::BOOLEAN,
            );
            let null = BoundExpr::constant(Value::null(return_type.clone()));
            return Some(
                BoundExpr::new(
                    BoundExprKind::Case {
                        checks: vec![(is_null, null)],
                        else_expr: Box::new(first),
                    },
                    return_type,
                )
                .with_alias(name.clone()),
            );
        }
        Some(BoundExpr::column_ref(
            ColumnBinding::new(state.group_index, group),
            state.groups[group].return_type.clone(),
            name.clone(),
        ))
    }

    /// `BaseSelectBinder::TryBindGroup`: the group `expr` matches.
    fn find_group(&self, expr: &Expr) -> Option<usize> {
        let state = self.aggregates.as_ref().filter(|s| !s.inside_aggregate)?;
        if state.group_map.is_empty() && state.group_alias_map.is_empty() {
            return None;
        }
        let aliased = match &expr.kind {
            ExprKind::ColumnRef(names) if names.len() == 1 => state
                .group_alias_map
                .iter()
                .find(|(alias, _)| alias.eq_ignore_ascii_case(&names[0]))
                .map(|(_, group)| *group),
            _ => None,
        };
        aliased.or_else(|| {
            let key = without_aliases(expr);
            state.group_map.iter().find(|(e, _)| *e == key).map(|(_, group)| *group)
        })
    }

    /// `BaseSelectBinder::BindGroupingFunction`: a column with the bitmask of a GROUPING call,
    /// which the aggregate computes per grouping set.
    fn bind_grouping_function(&mut self, children: &[Expr], name: String) -> Result<BoundExpr> {
        let Some(state) = self.aggregates.as_ref().filter(|s| !s.inside_aggregate) else {
            return Err(binder_error("GROUPING function is not supported here"));
        };
        if state.groups.is_empty() {
            return Err(binder_error("GROUPING statement cannot be used without groups"));
        }
        let mut groups: Vec<usize> = (0..state.groups.len()).collect();
        if !children.is_empty() {
            groups.clear();
            for child in children {
                let mut child = child.clone();
                self.qualify_column_names(&mut child);
                let Some(group) = self.find_group(&child) else {
                    return Err(binder_error(format!(
                        "GROUPING child {child} must be a grouping column"
                    )));
                };
                groups.push(group);
            }
        }
        if groups.len() >= 64 {
            return Err(binder_error("GROUPING statement cannot have more than 64 groups"));
        }
        let state = self.aggregates.as_mut().expect("checked above");
        let binding = ColumnBinding::new(state.groupings_index, state.grouping_functions.len());
        state.grouping_functions.push(groups);
        Ok(BoundExpr::column_ref(binding, LogicalType::BIGINT, name))
    }

    /// `HavingBinder::BindColumnRef`: a column that is no group names a select list alias;
    /// with GROUP BY ALL it becomes a group.
    fn bind_having_column(&mut self, names: &[String]) -> Result<BoundExpr> {
        let column = names.last().cloned().unwrap_or_default();
        if let [name] = names
            && let Some(position) = self
                .alias_fallback
                .iter()
                .position(|(a, _)| a.eq_ignore_ascii_case(name))
        {
            // (the aggregates of this query that a subquery reads are no aliases of the query)
            if self.binding_correlated && !name.starts_with("__outer_aggregate") {
                return Err(binder_error(format!(
                    "Having clause cannot reference alias {} in correlated subquery",
                    sql_quoted_identifier(name)
                )));
            }
            let (alias, aliased) = self.alias_fallback.remove(position);
            let bound = self.bind_expression(&aliased);
            self.alias_fallback.insert(position, (alias, aliased));
            return bound;
        }
        let state = self.aggregates.as_ref().expect("HAVING binds with a grouping");
        if !state.force_aggregates {
            return Err(binder_error(format!(
                "column {} must appear in the GROUP BY clause or be used in an aggregate function",
                sql_quoted_identifier(&column)
            )));
        }
        if self.binding_correlated {
            return Err(binder_error(format!(
                "Having clause cannot reference column {} in correlated subquery and group by all",
                sql_quoted_identifier(&column)
            )));
        }
        let group_index = state.group_index;
        let (binding, ty, name) = self.bind_context.bind_column_ref(names)?;
        let state = self.aggregates.as_mut().expect("HAVING binds with a grouping");
        let group = state.groups.len();
        state
            .groups
            .push(BoundExpr::column_ref(binding, ty.clone(), name.clone()));
        Ok(BoundExpr::column_ref(ColumnBinding::new(group_index, group), ty, name))
    }

    /// `InSearchPathFun`: the paths of `CatalogSearchPath::SetPathsInternal`, read when the query runs.
    fn bind_in_search_path(&mut self, function: &FunctionExpr) -> Result<BoundExpr> {
        let mut children = Vec::with_capacity(2);
        for argument in &function.args {
            children.push(
                self.bind_expression(&argument.expr)?
                    .cast_to(&LogicalType::VARCHAR, false),
            );
        }
        let (default_database, set_paths) = match &self.session {
            Some(session) => (session.database.clone(), session.search_entries.clone()),
            None => (self.catalog.name.clone(), Vec::new()),
        };
        let set_paths = if set_paths.is_empty() {
            vec![(String::new(), crate::catalog::DEFAULT_SCHEMA.to_string())]
        } else {
            set_paths
        };
        let paths: Vec<(String, String)> = std::iter::once(("temp".to_string(), "main".to_string()))
            .chain(set_paths)
            .chain([
                (String::new(), "main".to_string()),
                ("system".to_string(), "main".to_string()),
                ("system".to_string(), "pg_catalog".to_string()),
            ])
            .collect();
        // `CatalogSearchPath::SchemaInSearchPath`, on the bytes of the arguments
        let in_path = move |database: &[u8], schema: &[u8]| {
            paths.iter().any(|(catalog, path_schema)| {
                path_schema.as_bytes().eq_ignore_ascii_case(schema)
                    && (catalog.as_bytes().eq_ignore_ascii_case(database)
                        || (catalog.is_empty() && default_database.as_bytes().eq_ignore_ascii_case(database)))
            })
        };
        // `BinaryExecutor::Execute<string_t, string_t, bool>`: NULL where either argument is
        let bound: crate::function::BoundCallback = std::sync::Arc::new(move |args, result| {
            duckrs_common::executor::binary_str::<bool>(
                args.column(0),
                args.column(1),
                result,
                args.len(),
                |database, schema| in_path(database, schema),
            );
            Ok(())
        });
        let mut scalar = crate::function::ScalarFunction::new(
            "in_search_path",
            vec![LogicalType::VARCHAR, LogicalType::VARCHAR],
            LogicalType::BOOLEAN,
            |_, _| Err(crate::unported("in_search_path without its search path")),
        );
        scalar.bound = Some(bound);
        Ok(BoundExpr::new(
            BoundExprKind::Function {
                function: std::sync::Arc::new(scalar),
                children,
                is_operator: false,
            },
            LogicalType::BOOLEAN,
        ))
    }

    /// The functions that are constants of the session.
    fn bind_session_function(&mut self, function: &FunctionExpr) -> Result<Option<BoundExpr>> {
        let name = function.name.name.to_lowercase();
        // `CurrentDatabaseFunction`: the database the connection uses
        if name == "current_database" && function.args.is_empty() {
            let database = self
                .session
                .as_ref()
                .map_or_else(|| self.catalog.name.clone(), |s| s.database.clone());
            return Ok(Some(BoundExpr::constant(Value::varchar(database))));
        }
        if name == "current_schema" && function.args.is_empty() {
            // `CatalogSearchPath::GetDefault`: the first schema that was set
            let schema = self
                .session
                .as_ref()
                .and_then(|session| session.search_path.first().cloned())
                .unwrap_or_else(crate::catalog::default_schema);
            return Ok(Some(BoundExpr::constant(Value::varchar(schema))));
        }
        // `IndexKeyFun`
        if name == "index_key" && function.name.qualification.iter().all(String::is_empty) {
            return self.bind_index_key(function).map(Some);
        }
        // `VersionFun`: the version of the library
        if name == "version" && function.args.is_empty() && function.name.qualification.iter().all(String::is_empty) {
            return Ok(Some(BoundExpr::constant(Value::varchar(format!(
                "v{}",
                env!("CARGO_PKG_VERSION")
            )))));
        }
        // `InSearchPathFunction`: whether the schema of the database is on the search path
        if name == "in_search_path"
            && function.args.len() == 2
            && function.name.qualification.iter().all(String::is_empty)
        {
            return self.bind_in_search_path(function).map(Some);
        }
        if name == "current_query"
            && function.args.is_empty()
            && let Some(session) = &self.session
        {
            return Ok(Some(BoundExpr::constant(Value::varchar(session.current_query.clone()))));
        }
        // `CurrentSchemasBind`: the schemas that were set, or with `include_implicit` the whole search path
        if name == "current_schemas" && function.args.len() == 1 {
            let argument = self.bind_expression(&function.args[0].expr)?;
            // `CurrentSchemasResolveTypes`: a NULL takes the BOOLEAN parameter
            if !matches!(
                argument.return_type.id(),
                LogicalTypeId::Boolean | LogicalTypeId::SqlNull
            ) {
                return Err(binder_error("current_schemas requires a boolean input"));
            }
            // `BindFunctionInput::GetConstant`
            if !argument.is_foldable() {
                return Err(binder_error(
                    "The include_implicit argument in function current_schemas must be a constant expression",
                ));
            }
            let value = crate::execution::expression_executor::evaluate_scalar(&argument)?;
            let list_type = LogicalType::list(LogicalType::VARCHAR);
            if value.is_null() {
                return Ok(Some(BoundExpr::constant(Value::null(list_type))));
            }
            let set = self
                .session
                .as_ref()
                .map(|session| session.search_path.clone())
                .unwrap_or_default();
            let schemas: Vec<String> = if matches!(value.data(), duckrs_common::value::ValueData::Bool(true)) {
                std::iter::once("main".to_string())
                    .chain(set)
                    .chain(["main", "main", "pg_catalog"].map(str::to_string))
                    .collect()
            } else {
                set
            };
            let items = schemas.into_iter().map(Value::varchar).collect();
            return Ok(Some(BoundExpr::constant(Value::new(
                list_type,
                duckrs_common::value::ValueData::List(items),
            )?)));
        }
        if matches!(name.as_str(), "txid_current" | "current_transaction_id") && function.args.is_empty() {
            return Ok(Some(BoundExpr::constant(Value::ubigint(self.options.transaction_id))));
        }
        if matches!(name.as_str(), "current_connection_id" | "current_query_id")
            && function.args.is_empty()
            && let Some(session) = &self.session
        {
            let id = if name == "current_query_id" {
                session.query_id
            } else {
                session.connection_id
            };
            return Ok(Some(BoundExpr::constant(Value::ubigint(id))));
        }
        if !matches!(name.as_str(), "getvariable" | "current_setting") {
            return Ok(None);
        }
        let [argument] = function.args.as_slice() else {
            return Ok(None);
        };
        let bound = self.bind_expression(&argument.expr)?;
        if !bound.is_foldable() {
            return Err(binder_error(format!("{name} requires a constant input")));
        }
        let key = evaluate_scalar(&bound)?;
        if key.is_null() {
            if name == "current_setting" {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "Variable name to current_setting must be non-NULL",
                ));
            }
            return Ok(Some(BoundExpr::constant(Value::sql_null())));
        }
        let key = key.try_to_string()?;
        let session = self.session.clone();
        Ok(Some(BoundExpr::constant(match (name.as_str(), session) {
            ("getvariable", Some(session)) => session
                .variables
                .get(&key.to_lowercase())
                .cloned()
                .unwrap_or_else(Value::sql_null),
            ("getvariable", None) => Value::sql_null(),
            (_, Some(session)) => (session.setting)(&key)?,
            (_, None) => return Err(crate::unported("current_setting here")),
        })))
    }

    /// `BaseSelectBinder::BindAggregate`.
    fn bind_aggregate(&mut self, function: &FunctionExpr) -> Result<BoundExpr> {
        // `InaccessibleLambdaBindings`: the parameters of the lambdas around the aggregate are not
        // values of the rows it aggregates
        let before = std::mem::replace(&mut self.inaccessible_lambdas, self.lambda_scopes.len());
        let result = self.bind_aggregate_call(function);
        self.inaccessible_lambdas = before;
        result
    }

    fn bind_aggregate_call(&mut self, function: &FunctionExpr) -> Result<BoundExpr> {
        if self.aggregate_of_outer_query(&function.args.iter().map(|a| a.expr.clone()).collect::<Vec<_>>()) {
            self.pending_outer_aggregate = Some(Expr::from(ExprKind::Function(function.clone())));
            return Err(crate::unported(OUTER_AGGREGATE));
        }
        if self.inside_try {
            return Err(binder_error("aggregates are not allowed inside the TRY expression"));
        }
        let Some(state) = self.aggregates.as_mut() else {
            return Err(binder_error(self.aggregate_error));
        };
        if state.inside_aggregate {
            return Err(binder_error("aggregate function calls cannot be nested"));
        }
        state.bound_aggregate = true;
        state.inside_aggregate = true;
        let filter = match &function.filter {
            Some(filter) => Some(self.bind_expression(filter)?.cast_to(&LogicalType::BOOLEAN, false)),
            None => None,
        };
        let mut children = function
            .args
            .iter()
            .map(|a| self.bind_expression(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        let set = BuiltinFunctions::get()
            .aggregate(&function.name.name)
            .expect("checked by the caller");
        // ordered-set aggregates: the single ORDER BY expression moves to the front of the arguments
        let ordered_set = match (function.order_by.len(), function.name.name.as_str()) {
            (1, "mode") => 1,
            (1, "quantile_cont" | "quantile_disc") => 2,
            _ => 0,
        };
        if ordered_set == 2
            && let Some(parameter) = children.last_mut()
            && parameter.is_foldable()
        {
            let value = evaluate_scalar(parameter)?;
            if matches!(value.data(), ValueData::List(items) if items.is_empty()) {
                return Err(binder_error("Empty list in percentile not allowed"));
            }
            let node = &function.order_by[0];
            let order = self.options.resolve_order(node.order, node.nulls).order;
            let descending = order == duckrs_common::sort_key::OrderType::Descending;
            *parameter = BoundExpr::constant(negate_fractions(&value, descending)?);
        }
        let mut order_by = Vec::with_capacity(function.order_by.len());
        for node in &function.order_by {
            if !self.options.order_by_non_integer_literal
                && matches!(&node.expr.kind, ExprKind::Constant(literal) if literal.kind != LiteralKind::Integer)
            {
                return Err(binder_error(
                    "ORDER BY non-integer literal has no effect.\n* SET order_by_non_integer_literal=true to allow \
                     this behavior.\n\nPerhaps you misplaced ORDER BY; ORDER BY must appear after all regular \
                     arguments of the aggregate.",
                ));
            }
            let expr = self.bind_expression(&node.expr)?;
            order_by.push((expr, self.options.resolve_order(node.order, node.nulls)));
        }
        if ordered_set > 0 {
            if children.len() < ordered_set {
                children.insert(0, order_by[0].0.clone());
            }
            if ordered_set == 2 {
                order_by.clear();
            }
        }
        // `BaseSelectBinder::IsFunctionallyDependent`: the ORDER BY of a DISTINCT aggregate is
        // made of its arguments and constants
        fn functionally_dependent(expr: &BoundExpr, arguments: &[String]) -> bool {
            if expr.is_volatile() {
                return false;
            }
            if expr.is_foldable() || arguments.contains(&format!("{:?}", expr.kind)) {
                return true;
            }
            let children = expr.children();
            !children.is_empty()
                && children
                    .into_iter()
                    .all(|child| functionally_dependent(child, arguments))
        }
        let arguments: Vec<String> = children.iter().map(|child| format!("{:?}", child.kind)).collect();
        if function.distinct
            && !order_by
                .iter()
                .all(|(order, _)| functionally_dependent(order, &arguments))
        {
            return Err(binder_error(
                "In a DISTINCT aggregate, ORDER BY expressions must appear in the argument list",
            ));
        }
        // `BindMinMax`: min and max of a collated type are arg_min and arg_max of the value by its
        // collated value (`MinMaxUsesCollation`: VARCHAR, BIT, TIMETZ and the nested types)
        let mut set = set;
        let mut children = children;
        if matches!(set.name.as_str(), "min" | "max")
            && let Some(child) = children.first().filter(|_| children.len() <= 2)
            && matches!(
                child.return_type.id(),
                LogicalTypeId::Varchar
                    | LogicalTypeId::Bit
                    | LogicalTypeId::TimeTz
                    | LogicalTypeId::Struct
                    | LogicalTypeId::Tuple
                    | LogicalTypeId::List
                    | LogicalTypeId::Array
            )
            && !function.export_state
        {
            let ty = child.return_type.clone();
            let (collated, pushed) = crate::planner::collation::push_collation(
                child.clone(),
                &ty,
                &self.default_collation(),
                crate::planner::collation::CollationType::AllCollations,
            )?;
            if pushed {
                let name = if set.name == "min" { "arg_min" } else { "arg_max" };
                set = crate::function::BuiltinFunctions::get()
                    .aggregate(name)
                    .expect("arg_min and arg_max are aggregates");
                // `min(x, n)` is `arg_min(x, key, n)` (`MinMaxNBind`)
                children.insert(1, collated);
            }
        }
        // `ArgMinMaxBase::Bind` and `ArgMinMaxNBind`: the rows compare by the collated key
        else if matches!(
            set.name.as_str(),
            "arg_min"
                | "argmin"
                | "min_by"
                | "arg_max"
                | "argmax"
                | "max_by"
                | "arg_min_null"
                | "arg_max_null"
                | "arg_min_nulls_last"
                | "arg_max_nulls_last"
        ) && children.len() >= 2
            && !function.export_state
        {
            let key = children[1].clone();
            let ty = key.return_type.clone();
            children[1] = crate::planner::collation::push_collation(
                key,
                &ty,
                &self.default_collation(),
                crate::planner::collation::CollationType::AllCollations,
            )?
            .0;
        }
        // `FunctionBinder::BindAggregateFunction`: a DISTINCT count counts collated values
        let children = if function.distinct && set.name == "count" {
            children
                .into_iter()
                .map(|child| {
                    let ty = child.return_type.clone();
                    self.collate(child, &ty)
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            children
        };
        let mut aggregate = bind_aggregate_function(set, children, filter, function.distinct)?;
        // `MultiStageAggregateRewriter`: a DISTINCT aggregate (other than count, whose argument is
        // collated already) tells its rows apart by their collated values, and aggregates the values
        if aggregate.distinct
            && aggregate.function.name != "count"
            && let Some(child) = aggregate.children.first()
        {
            let ty = child.return_type.clone();
            let (key, pushed) = crate::planner::collation::push_collation(
                child.clone(),
                &ty,
                &self.default_collation(),
                crate::planner::collation::CollationType::AllCollations,
            )?;
            if pushed {
                aggregate.distinct_key = Some(key);
            }
        }
        // the keys as written, which an exported ordered state buffers
        let written_order: Vec<(BoundExpr, duckrs_common::sort_key::OrderModifiers)> = order_by.clone();
        if aggregate.distinct {
            let arguments: Vec<String> = aggregate.children.iter().map(|c| format!("{c:?}")).collect();
            let mut seen: Vec<String> = Vec::new();
            for (at, (expr, _)) in written_order.iter().enumerate() {
                let text = format!("{expr:?}");
                if !arguments.contains(&text) && !seen.contains(&text) {
                    seen.push(text);
                    aggregate.distinct_order.push(at);
                }
            }
        }
        // `BindAggregate`: the rows are ordered as the collation of their sort keys says
        aggregate.order_by = order_by
            .into_iter()
            .map(|(expr, modifiers)| {
                let ty = expr.return_type.clone();
                Ok((self.collate(expr, &ty)?, modifiers))
            })
            .collect::<Result<Vec<_>>>()?;
        if function.export_state {
            // the exported state does not keep the values seen so far
            if function.distinct {
                return Err(binder_error(
                    "EXPORT_STATE is not supported for DISTINCT aggregates: the exported state does not retain the \
                     set of distinct values, so combining states would produce incorrect results",
                ));
            }
            // the arguments of an aggregate that keeps its rows arrive packed into one STRUCT
            let packed = matches!(
                aggregate.function.kind,
                crate::function::aggregates::AggregateKind::Holistic(_)
            ) && aggregate.function.arguments.len() > 1;
            let arguments: &[BoundExpr] = match aggregate.children.as_slice() {
                [
                    BoundExpr {
                        kind: BoundExprKind::Function { children, .. },
                        ..
                    },
                ] if packed => children,
                children => children,
            };
            let types: Vec<LogicalType> = arguments.iter().map(|c| c.return_type.clone()).collect();
            // `EncodeStateParameters`: the constants the arguments after the first were bound to
            let takes_constants = matches!(
                aggregate.function.kind,
                crate::function::aggregates::AggregateKind::Holistic(
                    crate::function::holistic::Holistic::StringAgg
                        | crate::function::holistic::Holistic::QuantileCont
                        | crate::function::holistic::Holistic::QuantileDisc
                        | crate::function::holistic::Holistic::ApproxQuantile
                )
            );
            let constants: Vec<Option<Value>> = if takes_constants && arguments.len() > 1 {
                arguments
                    .iter()
                    .enumerate()
                    .map(|(at, argument)| {
                        (at > 0 && argument.is_foldable())
                            .then(|| crate::execution::expression_executor::evaluate_scalar(argument))
                            .transpose()
                    })
                    .collect::<Result<_>>()?
            } else {
                Vec::new()
            };
            if !aggregate.order_by.is_empty() {
                // `CreateSortedAggregateStateType`: the rows buffered as they arrive, the arguments and then the keys
                // that are not one of them, and sorted when the state is finalized
                let mut buffered: Vec<BoundExpr> = arguments.to_vec();
                let mut orders = Vec::with_capacity(written_order.len());
                for (expr, modifiers) in &written_order {
                    let key = format!("{:?}", expr.kind);
                    let column = match buffered.iter().position(|b| format!("{:?}", b.kind) == key) {
                        Some(column) => column,
                        None => {
                            buffered.push(expr.clone());
                            buffered.len() - 1
                        }
                    };
                    orders.push(duckrs_common::types::StateOrder {
                        column: column as u32,
                        descending: modifiers.order == duckrs_common::sort_key::OrderType::Descending,
                        nulls_first: modifiers.nulls == duckrs_common::sort_key::NullOrder::NullsFirst,
                    });
                }
                let names = (0..buffered.len()).map(|i| format!("v{i}")).collect();
                let pack = crate::function::nested::bind_struct_pack(names, buffered)?;
                let ty = LogicalType::aggregate_state_of(
                    &LogicalType::list(pack.return_type.clone()),
                    &aggregate.function.name,
                    duckrs_common::types::UNRESOLVED_OVERLOAD,
                    types,
                    aggregate.return_type.clone(),
                )
                .with_state_constants(constants)
                .with_state_orders(orders);
                let sorted = crate::function::aggregates::AggregateFunction {
                    name: aggregate.function.name.clone(),
                    arguments: vec![LogicalType::new(LogicalTypeId::Any)],
                    return_type: ty.clone(),
                    kind: crate::function::aggregates::AggregateKind::Holistic(
                        crate::function::holistic::Holistic::Sorted,
                    ),
                    export: Some(ty.clone()),
                };
                aggregate.function = Arc::new(sorted);
                aggregate.children = vec![pack];
                aggregate.order_by.clear();
                aggregate.return_type = ty;
            } else {
                let (exported, ty) = crate::function::aggregates::export_state(
                    &aggregate.function,
                    &aggregate.return_type,
                    &types,
                    constants,
                )?;
                aggregate.function = Arc::new(exported);
                aggregate.return_type = ty;
            }
        }
        let state = self.aggregates.as_mut().expect("still bound");
        state.inside_aggregate = false;
        // equal aggregates are computed once
        let key = format!(
            "{:?}{:?}{:?}{}{:?}{:?}{:?}",
            aggregate.function,
            aggregate.children,
            aggregate.filter,
            aggregate.distinct,
            aggregate.order_by,
            aggregate.distinct_key,
            aggregate.distinct_order
        );
        let index = match state.aggregate_keys.iter().position(|k| *k == key) {
            Some(index) => index,
            None => {
                state.aggregate_keys.push(key);
                state.aggregates.push(aggregate);
                state.aggregates.len() - 1
            }
        };
        Ok(BoundExpr::column_ref(
            ColumnBinding::new(state.aggregate_index, index),
            state.aggregates[index].return_type.clone(),
            function.name.name.clone(),
        ))
    }
}

/// An expression with all aliases removed: `ParsedExpression::Equals` ignores aliases.
pub(super) fn without_aliases(expr: &Expr) -> Expr {
    let mut expr = expr.clone();
    visit_expr_mut(&mut expr, &mut |e| e.alias.clear());
    expr
}

/// `ComparisonBind` + `BoundComparisonExpression::TryBindComparison`: both sides are cast to
/// the type they can be compared in.
pub(super) fn bind_comparison(op: ComparisonOp, left: BoundExpr, right: BoundExpr) -> Result<BoundExpr> {
    let left_type = expression_return_type(&left);
    let right_type = expression_return_type(&right);
    let is_equality = matches!(
        op,
        ComparisonOp::Equal | ComparisonOp::NotEqual | ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom
    );
    let input_type = try_comparison_type(&left_type, &right_type, is_equality).ok_or_else(|| {
        binder_error(format!(
            "Cannot compare values of type {left_type} and type {right_type} - an explicit cast is required"
        ))
    })?;
    Ok(BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(checked_cast(left, &input_type)?),
            right: Box::new(checked_cast(right, &input_type)?),
        },
        LogicalType::BOOLEAN,
    ))
}

/// A cast the executor can run, or the not-ported error.
pub(super) fn checked_cast(expr: BoundExpr, target: &LogicalType) -> Result<BoundExpr> {
    if !crate::function::cast::has_cast(&expr.return_type, target) {
        return Err(crate::unported(format!("casting {} to {target}", expr.return_type)));
    }
    Ok(expr.cast_to(target, false))
}

/// `BoundComparisonExpression::TryBindComparison`: the type two values compare in.
fn try_comparison_type(left: &LogicalType, right: &LogicalType, is_equality: bool) -> Option<LogicalType> {
    let mut input_type = if is_equality {
        force_max_logical_type(left, right)
    } else {
        try_max_logical_type(left, right)?
    };
    match input_type.id() {
        LogicalTypeId::Decimal => {
            // the maximum width and the maximum scale over width
            let props = [left, right].map(crate::function::combine_types::decimal_properties);
            if let [Some((lw, ls)), Some((rw, rs))] = props {
                let max_scale = ls.max(rs);
                let max_width_over_scale = (lw - ls).max(rw - rs);
                let width = (max_scale + max_width_over_scale)
                    .max(lw)
                    .max(rw)
                    .min(decimal::MAX_WIDTH);
                input_type = LogicalType::decimal(width, max_scale);
            }
        }
        LogicalTypeId::Varchar => {
            // for comparison with strings, we prefer to bind to the numeric types
            if left.id() != LogicalTypeId::Varchar && switch_varchar_comparison(left) {
                input_type = normalize_type(left);
            } else if right.id() != LogicalTypeId::Varchar && switch_varchar_comparison(right) {
                input_type = normalize_type(right);
            }
        }
        _ => {}
    }
    Some(normalize_type(&input_type))
}

impl Binder<'_> {
    /// `ExpressionBinder::BindExpression(CaseExpression)`.
    fn bind_case(&mut self, checks: &[(Expr, Expr)], else_expr: &Expr) -> Result<BoundExpr> {
        let mut bound = Vec::with_capacity(checks.len());
        for (when, then) in checks {
            bound.push((self.bind_expression(when)?, self.bind_expression(then)?));
        }
        let else_expr = self.bind_expression(else_expr)?;
        let mut return_type = expression_return_type(&else_expr);
        for (_, then) in &bound {
            let then_type = expression_return_type(then);
            return_type = try_max_logical_type(&return_type, &then_type).ok_or_else(|| {
                binder_error(format!(
                    "Cannot mix values of type {return_type} and {then_type} in CASE expression - an explicit cast \
                     is required"
                ))
            })?;
        }
        let return_type = normalize_type(&return_type);
        let mut checks = Vec::with_capacity(bound.len());
        for (when, then) in bound {
            checks.push((
                checked_cast(when, &LogicalType::BOOLEAN)?,
                checked_cast(then, &return_type)?,
            ));
        }
        let else_expr = Box::new(checked_cast(else_expr, &return_type)?);
        Ok(BoundExpr::new(BoundExprKind::Case { checks, else_expr }, return_type))
    }

    /// `ExpressionBinder::BindExpression(BetweenExpression)`.
    fn bind_between(&mut self, input: &Expr, lower: &Expr, upper: &Expr) -> Result<BoundExpr> {
        let (input, lower, upper) = (
            self.bind_expression(input)?,
            self.bind_expression(lower)?,
            self.bind_expression(upper)?,
        );
        let mix = |a: &LogicalType, b: &LogicalType| {
            binder_error(format!(
                "Cannot mix values of type {a} and {b} in BETWEEN clause - an explicit cast is required"
            ))
        };
        let input_type = expression_return_type(&input);
        let lower_type = expression_return_type(&lower);
        let upper_type = expression_return_type(&upper);
        let first =
            try_comparison_type(&input_type, &lower_type, false).ok_or_else(|| mix(&input_type, &lower_type))?;
        let target = try_comparison_type(&first, &upper_type, false).ok_or_else(|| mix(&first, &upper_type))?;
        let originals = [
            input.return_type.clone(),
            lower.return_type.clone(),
            upper.return_type.clone(),
        ];
        let sides = vec![
            checked_cast(input, &target)?,
            checked_cast(lower, &target)?,
            checked_cast(upper, &target)?,
        ];
        let [input, lower, upper]: [BoundExpr; 3] = self
            .collate_compared(&originals, sides)?
            .try_into()
            .expect("three sides");
        // a column can be copied cheaply: two comparisons, which the optimizers handle more easily, and
        // which the filter combiner folds into a BETWEEN again where both remain
        if matches!(input.kind, BoundExprKind::ColumnRef { .. }) {
            let compare = |op, left: BoundExpr, right: BoundExpr| {
                BoundExpr::new(
                    BoundExprKind::Comparison {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    LogicalType::BOOLEAN,
                )
            };
            return Ok(BoundExpr::new(
                BoundExprKind::Conjunction {
                    and: true,
                    children: vec![
                        compare(ComparisonOp::GreaterThanOrEqual, input.clone(), lower),
                        compare(ComparisonOp::LessThanOrEqual, input, upper),
                    ],
                },
                LogicalType::BOOLEAN,
            ));
        }
        Ok(BoundExpr::new(
            BoundExprKind::Between {
                input: Box::new(input),
                lower: Box::new(lower),
                upper: Box::new(upper),
            },
            LogicalType::BOOLEAN,
        ))
    }

    /// `ResolveCoalesceType`, for COALESCE and (NOT) IN: casts the children to their common type.
    fn resolve_coalesce_type(
        &mut self,
        children: &[Expr],
        in_operator: Option<&str>,
    ) -> Result<(Vec<BoundExpr>, LogicalType)> {
        let bound = children
            .iter()
            .map(|c| self.bind_expression(c))
            .collect::<Result<Vec<_>>>()?;
        let mut max_type = expression_return_type(&bound[0]);
        for child in &bound[1..] {
            let child_type = expression_return_type(child);
            let next = match in_operator {
                Some(_) => try_comparison_type(&max_type, &child_type, true),
                None => try_max_logical_type(&max_type, &child_type),
            };
            max_type = next.ok_or_else(|| {
                let clause = match in_operator {
                    Some(name) => format!("{name} clause"),
                    None => "COALESCE operator".to_string(),
                };
                binder_error(format!(
                    "Cannot mix values of type {max_type} and {child_type} in {clause} - an explicit cast is required"
                ))
            })?;
        }
        let max_type = normalize_type(&max_type);
        let bound = bound
            .into_iter()
            .map(|c| checked_cast(c, &max_type))
            .collect::<Result<Vec<_>>>()?;
        Ok((bound, max_type))
    }

    /// `x IN (a, b)`, which stays an IN through the filter pushdown; `InClauseRewriter` spells out
    /// the short lists left after it.
    fn bind_in(&mut self, children: &[Expr], negated: bool) -> Result<BoundExpr> {
        let name = if negated { "NOT IN" } else { "IN" };
        let (bound, max_type) = self.resolve_coalesce_type(children, Some(name))?;
        // `BindExpression(OperatorExpression)`: the children compare with the collation of their type
        let bound = bound
            .into_iter()
            .map(|child| self.collate(child, &max_type))
            .collect::<Result<Vec<_>>>()?;
        let op = if negated {
            BoundOperator::NotIn
        } else {
            BoundOperator::In
        };
        Ok(BoundExpr::new(
            BoundExprKind::Operator { op, children: bound },
            LogicalType::BOOLEAN,
        ))
    }
}

/// The functions with `FunctionCollationHandling::PUSH_COMBINABLE_COLLATIONS`, by every name.
const PUSH_COMBINABLE_COLLATIONS: [&str; 31] = [
    "~~",
    "!~~",
    "~~~",
    "~~*",
    "!~~*",
    "like_escape",
    "not_like_escape",
    "ilike_escape",
    "not_ilike_escape",
    "contains",
    "starts_with",
    "^@",
    "instr",
    "strpos",
    "position",
    "hash",
    "list_contains",
    "array_contains",
    "list_has",
    "array_has",
    "list_position",
    "list_indexof",
    "array_position",
    "array_indexof",
    "list_has_any",
    "array_has_any",
    "&&",
    "list_has_all",
    "array_has_all",
    "@>",
    "<@",
];

/// `SwitchVarcharComparison`: types a string is cast to when compared with them.
fn switch_varchar_comparison(ty: &LogicalType) -> bool {
    use LogicalTypeId::*;
    matches!(
        ty.id(),
        Boolean
            | TinyInt
            | SmallInt
            | Integer
            | BigInt
            | HugeInt
            | Float
            | Double
            | Decimal
            | UTinyInt
            | USmallInt
            | UInteger
            | UBigInt
            | UHugeInt
            | Date
            | Time
            | TimestampSec
            | TimestampMs
            | Timestamp
            | TimestampNs
            | Interval
            | TimestampTz
            | TimestampTzNs
            | TimeTz
            | IntegerLiteral
    )
}

// Ported from duckdb/src/function/scalar/system/parse_log_message.cpp @ rust-port-baseline
impl Binder<'_> {
    /// `parse_duckdb_log_message(log_type, message)`: the message of a structured log type as its
    /// STRUCT (a cast of its text), and of the others as `{'message': ...}`.
    fn bind_parse_log_message(&mut self, function: &ast::FunctionExpr) -> Result<BoundExpr> {
        let binder = |message: &str| Error::new(ErrorKind::Binder, message);
        let [log_type, message] = function.args.as_slice() else {
            return Err(binder("structured_log_schema: expects 1 argument"));
        };
        let bound = self.bind_expression(&log_type.expr)?;
        if !matches!(
            bound.return_type.id(),
            LogicalTypeId::Varchar | LogicalTypeId::StringLiteral
        ) || !bound.is_foldable()
        {
            return Err(binder("structured_log_schema: 'log_type' argument must be a string"));
        }
        let name = crate::execution::expression_executor::evaluate_scalar(&bound)?.try_to_string()?;
        let Some((name, _)) = crate::logging::lookup_log_type(&name) else {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("structured_log_schema: '{name}' not found"),
            ));
        };
        let text = self
            .bind_expression(&message.expr)?
            .cast_to(&LogicalType::VARCHAR, false);
        match crate::logging::log_type_schema(name) {
            Some(schema) => {
                let ty = self.bind_named_type(&LogicalType::unbound(schema.to_string()))?;
                Ok(text.cast_to(&ty, false))
            }
            None => {
                let mut pack = function.clone();
                pack.name.name = "struct_pack".into();
                pack.args = vec![ast::FunctionArgument {
                    name: "message".into(),
                    expr: message.expr.clone(),
                }];
                let _ = text;
                self.bind_expression(&Expr::from(ExprKind::Function(pack)))
            }
        }
    }
}

// Ported from `WriteLogResolveTypes` and `WriteLogBind` of
// duckdb/src/function/scalar/system/write_log.cpp @ rust-port-baseline
impl Binder<'_> {
    /// `write_log(message, [level := ..., log_type := ..., scope := ..., disable_logging := ...,
    /// return_value := ...])`: logs the message, and gives `return_value` or NULL.
    fn bind_write_log(&mut self, function: &ast::FunctionExpr) -> Result<BoundExpr> {
        let binder = |message: String| Error::new(ErrorKind::Binder, message);
        let Some(first) = function.args.first() else {
            return Err(binder("write_log takes at least one argument".into()));
        };
        let message = self.bind_expression(&first.expr)?;
        if !matches!(
            message.return_type.id(),
            LogicalTypeId::Varchar | LogicalTypeId::StringLiteral
        ) || message.return_type.alias().is_some()
        {
            return Err(Error::new(
                ErrorKind::InvalidType,
                "write_log first argument must be a VARCHAR",
            ));
        }
        let message = message.cast_to(&LogicalType::VARCHAR, false);
        let (mut level, mut log_type, mut scope, mut disabled) =
            ("INFO".to_string(), String::new(), String::new(), false);
        let mut output = None;
        for argument in &function.args[1..] {
            let bound = self.bind_expression(&argument.expr)?;
            let is = |id: LogicalTypeId| {
                bound.return_type.id() == id
                    || (id == LogicalTypeId::Varchar && bound.return_type.id() == LogicalTypeId::StringLiteral)
            };
            let text = |bound: &BoundExpr| -> Result<String> {
                crate::execution::expression_executor::evaluate_scalar(bound)?.try_to_string()
            };
            match argument.name.as_str() {
                "disable_logging" => {
                    if !is(LogicalTypeId::Boolean) {
                        return Err(binder("write_log: 'disable_logging' argument must be a boolean".into()));
                    }
                    disabled = text(&bound)? == "true";
                }
                "scope" => {
                    if !is(LogicalTypeId::Varchar) {
                        return Err(binder("write_log: 'scope' argument must be a string".into()));
                    }
                    scope = text(&bound)?;
                }
                "level" => {
                    if !is(LogicalTypeId::Varchar) {
                        return Err(binder("write_log: 'level' argument must be a string".into()));
                    }
                    level = crate::logging::LogLevel::parse(&text(&bound)?)?.name().to_string();
                }
                "log_type" => {
                    if !is(LogicalTypeId::Varchar) {
                        return Err(binder("write_log: 'log_type' argument must be a string".into()));
                    }
                    log_type = text(&bound)?;
                }
                "return_value" => output = Some(bound),
                name => return Err(binder(format!("write_log: Unknown argument {name}"))),
            }
        }
        let Some(session) = self.session.clone() else {
            return Err(crate::unported("write_log here"));
        };
        let return_type = output.as_ref().map_or(LogicalType::VARCHAR, |o| o.return_type.clone());
        let mut children = vec![
            message,
            BoundExpr::constant(Value::ubigint(session.log_target)),
            BoundExpr::constant(Value::varchar(level)),
            BoundExpr::constant(Value::varchar(log_type)),
            BoundExpr::constant(Value::varchar(scope)),
            BoundExpr::constant(Value::boolean(disabled)),
        ];
        children.extend(output);
        let arguments = children.iter().map(|c| c.return_type.clone()).collect();
        let scalar = crate::function::ScalarFunction::new(
            "write_log",
            arguments,
            return_type.clone(),
            crate::logging::write_log,
        );
        Ok(BoundExpr::new(
            BoundExprKind::Function {
                function: Arc::new(scalar),
                children,
                is_operator: false,
            },
            return_type,
        ))
    }
}

/// `SelectBinder::TryResolveAliasReference`: an alias of a select list entry with a subquery.
fn alias_subquery_error(name: &str) -> Error {
    binder_error(format!(
        "Alias \"{name}\" referenced in a SELECT clause - but the expression has a subquery. This is not yet \
         supported."
    ))
}

/// `Value::TYPE`: a type as a value of the TYPE type, which keeps it serialized.
pub(crate) fn type_value(ty: &LogicalType) -> Value {
    let bytes = duckrs_common::serializer::BinarySerializer::serialize(
        ty,
        duckrs_common::serializer::SerializationOptions::default(),
    );
    Value::new(LogicalType::new(LogicalTypeId::Type), ValueData::Bytes(bytes)).expect("TYPE is stored as bytes")
}

/// An identifier as SQL spells it: quoted unless it needs no quotes.
fn quote_identifier(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `AliasFunction`: the name it was bound with, for every row.
fn alias_function(args: &duckrs_common::vector::DataChunk, result: &mut duckrs_common::vector::Vector) -> Result<()> {
    let name = args.column(1).get_value(0);
    for row in 0..args.len() {
        result.set_value(row, &name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_typed_like_duckdb() {
        let ty = |s: &str| number_to_value(s).unwrap().logical_type().clone();
        assert_eq!(ty("42"), LogicalType::INTEGER);
        assert_eq!(ty("-2147483648"), LogicalType::BIGINT);
        assert_eq!(ty("3000000000"), LogicalType::BIGINT);
        assert_eq!(ty("1_000"), LogicalType::INTEGER);
        assert_eq!(ty("99999999999999999999"), LogicalType::HUGEINT);
        assert_eq!(ty("1.5"), LogicalType::decimal(2, 1));
        assert_eq!(ty("-0.25"), LogicalType::decimal(3, 2));
        assert_eq!(ty(".5"), LogicalType::decimal(1, 1));
        assert_eq!(ty("1e3"), LogicalType::DOUBLE);
        assert_eq!(number_to_value("-0.25").unwrap().try_to_string().unwrap(), "-0.25");
    }
}
