// Ported from duckdb/src/planner/binder/expression/bind_{columnref,comparison,conjunction,
// constant,cast,function,operator}_expression.cpp, duckdb/src/parser/literal.cpp
// (NumberToValue), duckdb/src/function/scalar/comparison/comparison.cpp (ComparisonBind) and
// duckdb/src/catalog/default/default_types.cpp @ rust-port-baseline
//
//! Binding parsed expressions into [`BoundExpr`]s.

use super::{Binder, ColumnMode, CorrelatedColumn, binder_error};
use crate::catalog::DEFAULT_CATALOG;
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
use duckrs_parser::transformer::{visit_expr_mut, visit_query_node_exprs_mut};
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
fn type_parameter_type(expr: &Expr) -> &'static str {
    match &expr.kind {
        ExprKind::Constant(literal) => match literal.kind {
            LiteralKind::Integer => "INTEGER",
            LiteralKind::String => "VARCHAR",
            LiteralKind::Null => "NULL",
            _ => "DECIMAL",
        },
        // a negative number is a parenthesized expression here
        _ => "INTEGER",
    }
}

/// `ColumnQualifier::SQLValueFunctionName`: what these words call when no column has their name.
fn sql_value_function(name: &str) -> Option<&'static str> {
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

impl Binder<'_> {
    /// `Binder::BindLogicalType` for the types slice 1 executes: the scalar types without
    /// parameters, `VARCHAR(n)` and `DECIMAL(width, scale)`.
    /// A type of a database file that names a type of the catalog: its SQL is what would name it
    /// in a query.
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
        let (original, children): (&str, &[Expr]) = match target {
            CastType::Resolved(ty) if ty.contains_unbound() => return self.bind_named_type(ty),
            CastType::Resolved(ty) => return Ok(ty.clone()),
            CastType::Builtin(name) => (&**name, &[]),
            CastType::Parsed(expr) => match &expr.kind {
                ExprKind::Type(TypeExpr { name, children }) => {
                    if !name.qualification.is_empty() {
                        let schema = self.schema_of(name)?;
                        return self
                            .catalog
                            .get_type(schema.as_deref(), None, &name.name)?
                            .ok_or_else(|| {
                                Error::new(
                                    ErrorKind::Catalog,
                                    format!("Type with name {} does not exist!", name.name),
                                )
                            });
                    }
                    (name.name.as_str(), children.as_slice())
                }
                _ => return Err(crate::unported(format!("type expression {expr}"))),
            },
        };
        let name = original.to_lowercase();
        if let Some(ty) = self.catalog.get_type(None, self.search_schema.as_deref(), &name)? {
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
            let hint = closest.map_or_else(String::new, |(_, type_name)| format!("\nDid you mean \"{type_name}\"?"));
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Type with name {original} does not exist!{hint}"),
            ));
        };
        match id {
            LogicalTypeId::Decimal => {
                let rejected = || {
                    let types: Vec<&str> = children.iter().map(type_parameter_type).collect();
                    binder_error(format!(
                        "Type \"DECIMAL\" does not accept type parameters ({})\nCandidates:\n    DECIMAL\n    \
                         DECIMAL(width UTINYINT, scale UTINYINT := 0)",
                        types.join(", ")
                    ))
                };
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
            // the width of a VARCHAR is accepted and ignored
            LogicalTypeId::Varchar if children.len() <= 1 => Ok(LogicalType::VARCHAR),
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
                            let types: Vec<&str> = children.iter().map(type_parameter_type).collect();
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
            _ if !children.is_empty() => Err(crate::unported(format!("type parameters for {}", name.to_uppercase()))),
            LogicalTypeId::Type => Err(crate::unported(format!("the {} type", name.to_uppercase()))),
            id => Ok(LogicalType::new(id)),
        }
    }

    /// The type constructors of `default_types.cpp` for the nested types.
    fn bind_nested_type(&self, name: &str, id: LogicalTypeId, children: &[Expr]) -> Result<LogicalType> {
        let upper = name.to_uppercase();
        let child_type = |expr: &Expr| self.bind_type(&CastType::Parsed(Box::new(expr.clone())));
        let rejected = || {
            let types: Vec<&str> = children.iter().map(type_parameter_type).collect();
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
    fn aggregate_of_outer_query(&self, function: &FunctionExpr) -> bool {
        let (mut own, mut outer) = (false, false);
        for argument in &function.args {
            visit_expr_mut(&mut argument.expr.clone(), &mut |e| {
                if let ExprKind::ColumnRef(names) = &e.kind {
                    if self.bind_context.bind_column_ref(names).is_ok() {
                        own = true;
                    } else if self.outer_contexts.iter().any(|c| c.bind_column_ref(names).is_ok()) {
                        outer = true;
                    }
                }
            });
        }
        outer && !own
    }

    /// `Binder::BindCorrelatedColumns`: a column of a query around this one.
    fn bind_outer_column(&mut self, names: &[String]) -> Option<BoundExpr> {
        for context in self.outer_contexts.iter().rev() {
            let Ok((binding, ty, name)) = context.bind_column_ref(names) else {
                continue;
            };
            let alias = context
                .bindings
                .iter()
                .find(|b| b.table_index == binding.table_index)?
                .alias
                .clone();
            let mut correlated = self.correlated.lock();
            if !correlated.iter().any(|c| c.binding == binding) {
                correlated.push(CorrelatedColumn {
                    binding,
                    names: vec![alias, name.clone()],
                });
            }
            let mut expr = BoundExpr::column_ref(binding, ty, name);
            expr.kind = BoundExprKind::ColumnRef { binding, depth: 1 };
            return Some(expr);
        }
        None
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
        let (binder, bound) = loop {
            let mut context = self.bind_context.clone();
            if !outer_aggregates.is_empty() {
                let table_index = self.generate_table_index();
                context.add_binding(super::Binding {
                    alias: "__outer_aggregates".to_string(),
                    schema: None,
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
                    let Some(function) = binder.pending_outer_aggregate.take() else {
                        return Err(self.correlated_error(error));
                    };
                    self.next_table_index = binder.next_table_index;
                    let expr = Expr::from(ExprKind::Function(function));
                    let bound = self.bind_expression(&expr)?;
                    let name = format!("__outer_aggregate_{}", outer_aggregates.len());
                    let ty = bound.return_type.clone();
                    visit_query_node_exprs_mut(&mut rewritten.node, &mut |e| {
                        if e.kind == expr.kind {
                            let alias = std::mem::take(&mut e.alias);
                            *e = Expr::column(vec![name.clone()]);
                            e.alias = alias;
                        }
                    });
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
            let bound = self.bind_expression(&Expr::from(ExprKind::ColumnRef(names)));
            match bound {
                Ok(bound) => correlated.push(bound),
                Err(error) => {
                    self.alias_fallback.truncate(fallback_size);
                    return Err(error);
                }
            }
            bindings.push(column.binding);
        }
        self.alias_fallback.truncate(fallback_size);
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
                    raw: (!volatile).then_some(raw),
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
        let bound = self.bind_expression_inner(expr)?;
        if matches!(expr.kind, ExprKind::Function(_) | ExprKind::Operator { .. })
            && matches!(bound.kind, BoundExprKind::Function { .. })
        {
            return self.function_collations(bound);
        }
        Ok(bound)
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
        if matches!(function.name.as_str(), "list_sort" | "array_sort") && children.len() <= 3 {
            let ty = children[0].return_type.clone();
            let (keys, collated) = push_collation(
                children[0].clone(),
                &ty,
                &self.default_collation(),
                CollationType::AllCollations,
            )?;
            if collated {
                let option = |text: &str| BoundExpr::constant(Value::varchar(text));
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
        let (schema, table) = match names {
            [table] => (None, table),
            [schema, table] => (Some(schema.as_str()), table),
            _ => return None,
        };
        let bindings = self.bind_context.bindings_of(schema, table).ok()?;
        let [binding] = bindings.as_slice() else {
            return None;
        };
        let args = binding
            .names
            .iter()
            .map(|column| ast::FunctionArgument {
                name: column.clone(),
                expr: Expr::column(vec![binding.alias.clone(), column.clone()]),
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
                syntax: ast::LambdaSyntax::Arrow | ast::LambdaSyntax::StoredArrow,
            } => {
                let children = vec![self.bind_expression(lhs)?, self.bind_expression(expr)?];
                let types = std::cell::RefCell::new(self.child_binder());
                return crate::function::json::bind_function("->", children, &|name| types.borrow_mut().csv_type(name));
            }
            ExprKind::ColumnRef(names) if self.table_function_argument => {
                BoundExpr::constant(Value::varchar(names.join(".")))
            }
            ExprKind::ColumnRef(names) if !self.lambda_scopes.is_empty() && self.is_lambda_parameter(names) => {
                return Ok(self.bind_lambda_parameter(names)?.expect("just checked"));
            }
            ExprKind::ColumnRef(names) if self.bind_context.struct_field_start(names).is_some() => {
                let start = self
                    .bind_context
                    .struct_field_start(names)
                    .expect("checked by the guard");
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
                    let children = vec![bound, key];
                    bound = bind_nested_function("struct_extract", vec![String::new(); 2], children)?;
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
                if names.len() <= 2
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
                if !is_own && let Some(outer) = self.bind_outer_column(names) {
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
                let child = crate::function::binder::inside_try(|| self.bind_expression(&children[0]))?;
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
                let name = if *op == OperatorType::StructExtract {
                    "struct_extract"
                } else {
                    "array_extract"
                };
                let bound = children
                    .iter()
                    .map(|c| self.bind_expression(c))
                    .collect::<Result<Vec<_>>>()?;
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
                self.bind_aggregate(function)?
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
                if function.name.name.eq_ignore_ascii_case("if") {
                    let [condition, then, otherwise] = function.args.as_slice() else {
                        return Err(binder_error("if expects three arguments"));
                    };
                    return self.bind_case(&[(condition.expr.clone(), then.expr.clone())], &otherwise.expr);
                }
                if is_generic_function(&function.name.name) || is_unnest(&function.name.name) {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    if is_unnest(&function.name.name) {
                        return bind_unnest(children);
                    }
                    return bind_generic_function(&function.name.name, children);
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
                    return crate::function::extras::bind_function(&function.name.name, &names, children);
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
                    return bind_nested_function(&function.name.name, names, children);
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
                let Some(set) = BuiltinFunctions::get().scalar(&function.name.name) else {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!("Scalar Function with name {} does not exist!", function.name.name),
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
                        let name = if set.name == "strftime" { "strftime" } else { "strptime" };
                        return Err(binder_error(format!("{name} format must be a constant")));
                    }
                    // `RegexpExtractAllStruct` / `RegexExtractBind`: a list of group names makes a STRUCT
                    ("regexp_extract" | "regexp_extract_all", [_, _, groups, ..])
                        if children.len() <= 4 && groups.return_type.id() == LogicalTypeId::List =>
                    {
                        crate::function::regexp::bind_regexp_struct(&set.name, children)?
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
                            let bound =
                                bind_scalar_function(set, children, function.is_operator, &try_evaluate_scalar)?;
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
                let Some(parameters) = &self.parameters else {
                    // a statement run directly has no values for its parameters
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("Values were not provided for the following parameters: {identifier}"),
                    ));
                };
                let key = identifier.to_lowercase();
                let mut seen = parameters.seen.lock();
                if !seen.contains(&key) {
                    seen.push(key.clone());
                }
                let value = parameters.values.as_ref().and_then(|values| values.get(&key));
                BoundExpr::constant(value.cloned().unwrap_or_else(Value::sql_null))
            }
            ExprKind::Star(_) => return Err(binder_error("STAR expression is not supported here")),
            ExprKind::Default => return Err(binder_error("DEFAULT is not allowed here!")),
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
                | "txid_current"
                | "current_transaction_id"
                | "current_connection_id"
                | "current_query_id"
                | "getvariable"
                | "current_setting"
        )
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

/// `QualifiedName::TryParse`: the dot-separated, optionally double-quoted parts of a name.
fn parse_qualified_name(input: &str) -> Result<Vec<String>> {
    let error = |what: &str| {
        Error::new(
            ErrorKind::Parser,
            format!("Zero-length {what} in qualified name! (input: {input})"),
        )
    };
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
            }
            '"' if quoted => {
                if current.is_empty() {
                    return Err(error("delimited identifier"));
                }
                quoted = false;
            }
            '"' if current.is_empty() => quoted = true,
            '.' if !quoted => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if quoted {
        return Err(Error::new(
            ErrorKind::Parser,
            format!("Unterminated quote in qualified name! (input: {input})"),
        ));
    }
    parts.push(current);
    if parts.iter().any(String::is_empty) {
        return Err(error("identifier"));
    }
    Ok(parts)
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
        let schema = self.schema_of(&ast::QualifiedName {
            qualification: qualification.to_vec(),
            name: name.clone(),
        })?;
        let sequence = self
            .catalog
            .get_sequence(schema.as_deref(), self.search_schema.as_deref(), name)?;
        Ok(BoundExpr::new(
            BoundExprKind::Sequence {
                op,
                sequence,
                arguments: children[1..].to_vec(),
            },
            LogicalType::BIGINT,
        ))
    }

    /// `BaseSelectBinder::TryBindGroup` + `BindGroup`: a reference to the group `expr` matches.
    pub(super) fn try_bind_group(&self, expr: &Expr) -> Option<BoundExpr> {
        let group = self.find_group(expr)?;
        let state = self.aggregates.as_ref()?;
        if let Some((_, aggregate)) = state.collated_groups.iter().find(|(g, _)| *g == group) {
            let return_type = state.aggregates[*aggregate].return_type.clone();
            let first = BoundExpr::column_ref(
                ColumnBinding::new(state.aggregate_index, *aggregate),
                return_type.clone(),
                expr.alias.clone(),
            );
            if state.grouping_set_count <= 1 {
                return Some(first);
            }
            // with grouping sets, NULL where the set does not have the group
            let collated = BoundExpr::column_ref(
                ColumnBinding::new(state.group_index, group),
                state.groups[group].return_type.clone(),
                expr.alias.clone(),
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
                .with_alias(expr.alias.clone()),
            );
        }
        Some(BoundExpr::column_ref(
            ColumnBinding::new(state.group_index, group),
            state.groups[group].return_type.clone(),
            expr.alias.clone(),
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
        let group_index = state.group_index;
        let (binding, ty, name) = self.bind_context.bind_column_ref(names)?;
        let state = self.aggregates.as_mut().expect("HAVING binds with a grouping");
        let group = state.groups.len();
        state
            .groups
            .push(BoundExpr::column_ref(binding, ty.clone(), name.clone()));
        Ok(BoundExpr::column_ref(ColumnBinding::new(group_index, group), ty, name))
    }

    /// The functions that are constants of the session.
    fn bind_session_function(&mut self, function: &FunctionExpr) -> Result<Option<BoundExpr>> {
        let name = function.name.name.to_lowercase();
        // `CurrentDatabaseFunction`: the database the connection uses
        if name == "current_database" && function.args.is_empty() {
            return Ok(Some(BoundExpr::constant(Value::varchar(self.catalog.name.clone()))));
        }
        if name == "current_schema" && function.args.is_empty() {
            return Ok(Some(BoundExpr::constant(Value::varchar("main"))));
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
        if self.aggregate_of_outer_query(function) {
            self.pending_outer_aggregate = Some(function.clone());
            return Err(crate::unported("aggregates of the columns of an outer query"));
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
            if matches!(&node.expr.kind, ExprKind::Constant(literal) if literal.kind != LiteralKind::Integer) {
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
        if function.distinct
            && !order_by.iter().all(|(order, _)| {
                children
                    .iter()
                    .any(|child| format!("{:?}", child.kind) == format!("{:?}", order.kind))
            })
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
            let arguments: Vec<LogicalType> = aggregate.children.iter().map(|c| c.return_type.clone()).collect();
            let (exported, ty) =
                crate::function::aggregates::export_state(&aggregate.function, &aggregate.return_type, &arguments)?;
            aggregate.function = Arc::new(exported);
            aggregate.return_type = ty;
        }
        let state = self.aggregates.as_mut().expect("still bound");
        state.inside_aggregate = false;
        // equal aggregates are computed once
        let key = format!(
            "{:?}{:?}{:?}{}{:?}{:?}",
            aggregate.function,
            aggregate.children,
            aggregate.filter,
            aggregate.distinct,
            aggregate.order_by,
            aggregate.distinct_key
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

    /// `x IN (a, b)` as `x = a OR x = b`, which has its three-valued logic.
    fn bind_in(&mut self, children: &[Expr], negated: bool) -> Result<BoundExpr> {
        let name = if negated { "NOT IN" } else { "IN" };
        let (bound, max_type) = self.resolve_coalesce_type(children, Some(name))?;
        // `BindExpression(OperatorExpression)`: the children compare with the collation of their type
        let mut bound = bound
            .into_iter()
            .map(|child| self.collate(child, &max_type))
            .collect::<Result<Vec<_>>>()?;
        let input = bound.remove(0);
        let comparisons = bound
            .into_iter()
            .map(|value| {
                BoundExpr::new(
                    BoundExprKind::Comparison {
                        op: ComparisonOp::Equal,
                        left: Box::new(input.clone()),
                        right: Box::new(value),
                    },
                    LogicalType::BOOLEAN,
                )
            })
            .collect();
        let result = BoundExpr::new(
            BoundExprKind::Conjunction {
                and: false,
                children: comparisons,
            },
            LogicalType::BOOLEAN,
        );
        Ok(if negated {
            BoundExpr::new(
                BoundExprKind::Operator {
                    op: BoundOperator::Not,
                    children: vec![result],
                },
                LogicalType::BOOLEAN,
            )
        } else {
            result
        })
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
