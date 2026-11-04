// Ported from duckdb/src/planner/binder/expression/bind_{columnref,comparison,conjunction,
// constant,cast,function,operator}_expression.cpp, duckdb/src/parser/literal.cpp
// (NumberToValue), duckdb/src/function/scalar/comparison/comparison.cpp (ComparisonBind) and
// duckdb/src/catalog/default/default_types.cpp @ rust-port-baseline
//
//! Binding parsed expressions into [`BoundExpr`]s.

use super::{Binder, ColumnMode, CorrelatedColumn, binder_error, schema_of};
use crate::catalog::DEFAULT_CATALOG;
use crate::execution::expression_executor::{evaluate_scalar, try_evaluate_scalar};
use crate::function::binder::{bind_aggregate_function, bind_scalar_function, expression_return_type};
use crate::function::combine_types::{force_max_logical_type, normalize_type, try_max_logical_type};
use crate::function::generic::{bind_generic_function, bind_unnest, is_generic_function, is_unnest};
use crate::function::nested::{bind_nested_function, is_nested_function};
use crate::function::variant::{bind_variant_function, is_variant_function};
use crate::function::{BuiltinFunctions, typeof_constant};
use crate::planner::column_binding_resolver::resolve_column_bindings;
use crate::planner::expression::{
    BoundExpr, BoundExprKind, BoundOperator, BoundSubquery, ColumnBinding, SequenceOp, SubqueryKind,
};
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
fn literal_to_value(literal: &Literal) -> Result<Value> {
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

impl Binder<'_> {
    /// `Binder::BindLogicalType` for the types slice 1 executes: the scalar types without
    /// parameters, `VARCHAR(n)` and `DECIMAL(width, scale)`.
    pub fn bind_type(&self, target: &CastType) -> Result<LogicalType> {
        let (name, children): (String, &[Expr]) = match target {
            CastType::Builtin(name) => (name.to_lowercase(), &[]),
            CastType::Parsed(expr) => match &expr.kind {
                ExprKind::Type(TypeExpr { name, children }) => {
                    if !name.qualification.is_empty() {
                        let schema = schema_of(name)?;
                        return self.catalog.get_type(schema, None, &name.name)?.ok_or_else(|| {
                            Error::new(
                                ErrorKind::Catalog,
                                format!("Type with name {} does not exist!", name.name),
                            )
                        });
                    }
                    (name.name.to_lowercase(), children.as_slice())
                }
                _ => return Err(crate::unported(format!("type expression {expr}"))),
            },
        };
        if let Some(ty) = self.catalog.get_type(None, self.search_schema.as_deref(), &name)? {
            return Ok(ty);
        }
        let Some(id) = builtin_type(&name) else {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Type with name {name} does not exist!"),
            ));
        };
        match id {
            LogicalTypeId::Decimal => {
                let rejected = || {
                    let types: Vec<&str> = children.iter().map(type_parameter_type).collect();
                    binder_error(format!(
                        "Type \"DECIMAL\" does not accept type parameters ({})",
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
            // a string parameter is a collation
            LogicalTypeId::Varchar
                if children
                    .iter()
                    .any(|c| matches!(&c.kind, ExprKind::Constant(l) if l.kind == LiteralKind::String)) =>
            {
                Err(crate::unported("COLLATE"))
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
            LogicalTypeId::Geometry | LogicalTypeId::Type => {
                Err(crate::unported(format!("the {} type", name.to_uppercase())))
            }
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
        let mut binder = self.child_binder();
        binder.outer_contexts.push(self.bind_context.clone());
        binder.correlated = Arc::default();
        let bound = binder
            .bind_select_statement(subquery)
            .map_err(|error| self.correlated_error(error))?;
        self.next_table_index = binder.next_table_index;
        // the correlated columns are columns of this query, or of one around it
        let columns: Vec<CorrelatedColumn> = binder.correlated.lock().clone();
        let mut correlated = Vec::with_capacity(columns.len());
        let mut bindings = Vec::with_capacity(columns.len());
        for column in columns {
            correlated.push(self.bind_expression(&Expr::from(ExprKind::ColumnRef(column.names)))?);
            bindings.push(column.binding);
        }
        let child = child.map(|c| self.bind_expression(c)).transpose()?;
        let columns = bound.types.len();
        let row_width = child
            .as_ref()
            .filter(|c| c.return_type.id() == LogicalTypeId::Tuple)
            .map(|c| c.return_type.struct_children().len());
        if subquery_type != SubqueryType::Exists && subquery_type != SubqueryType::NotExists {
            match row_width {
                Some(width) if columns == width => return Err(crate::unported("subqueries compared to rows")),
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
        let (kind, return_type, mut plan, child) = match subquery_type {
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
                let plan = self.cast_plan(bound, std::slice::from_ref(&compare_type), vec![Some(0)]);
                (
                    SubqueryKind::Any(comparison),
                    LogicalType::BOOLEAN,
                    plan,
                    Some(Box::new(child)),
                )
            }
        };
        resolve_column_bindings(&mut plan)?;
        let volatile = plan.is_volatile();
        let expression = BoundExpr::new(
            BoundExprKind::Subquery {
                subquery: Arc::new(BoundSubquery {
                    kind,
                    plan,
                    correlated: bindings,
                    volatile,
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
        if let Some(group) = self.try_bind_group(expr) {
            return Ok(group);
        }
        if let ExprKind::Function(function) = &expr.kind
            && let Some(entry) = self.find_macro(&function.name, false)?
        {
            return self.bind_macro(function, &entry);
        }
        let bound = match &expr.kind {
            ExprKind::Constant(literal) => BoundExpr::constant(literal_to_value(literal)?),
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
                    if !matches!(
                        bound.return_type.id(),
                        LogicalTypeId::Struct | LogicalTypeId::Tuple | LogicalTypeId::Union | LogicalTypeId::Variant
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
                let (binding, ty, name) = match names.as_slice() {
                    [column] => match self.bind_context.bind_column(column) {
                        Ok(bound) => bound,
                        Err(error) => {
                            // a column that is not in the FROM clause may name a select list alias
                            let alias = self
                                .alias_fallback
                                .iter()
                                .position(|(a, _)| a.eq_ignore_ascii_case(column));
                            let Some(position) = alias else {
                                return Err(self.lateral_error(names, error));
                            };
                            // an alias may not refer to itself
                            let (_, aliased) = self.alias_fallback.remove(position);
                            let bound = self.bind_expression(&aliased);
                            self.alias_fallback.insert(position, (column.clone(), aliased));
                            return bound;
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
                bind_comparison(*op, left, right)?
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
                let child = self.bind_expression(&children[0])?;
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
                    return Err(crate::unported("qualified function names"));
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
                if is_variant_function(&function.name.name) {
                    let children = function
                        .args
                        .iter()
                        .map(|a| self.bind_expression(&a.expr))
                        .collect::<Result<Vec<_>>>()?;
                    return bind_variant_function(&function.name.name, children);
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
                    _ => {
                        let bound = bind_scalar_function(set, children, function.is_operator, &try_evaluate_scalar)?;
                        self.bind_sequence_function(bound)?
                    }
                }
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
        let (schema, name) = match parts.as_slice() {
            [name] => (None, name),
            [schema, name] => (Some(schema.as_str()), name),
            [catalog, schema, name] if catalog.eq_ignore_ascii_case(DEFAULT_CATALOG) => (Some(schema.as_str()), name),
            _ => return Err(crate::unported("this sequence name")),
        };
        let sequence = self.catalog.get_sequence(schema, self.search_schema.as_deref(), name)?;
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
        let group = aliased.or_else(|| {
            let key = without_aliases(expr);
            state.group_map.iter().find(|(e, _)| *e == key).map(|(_, group)| *group)
        })?;
        Some(BoundExpr::column_ref(
            ColumnBinding::new(state.group_index, group),
            state.groups[group].return_type.clone(),
            expr.alias.clone(),
        ))
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
        if name == "current_schema" && function.args.is_empty() {
            return Ok(Some(BoundExpr::constant(Value::varchar("main"))));
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
            return Err(crate::unported("aggregates of the columns of an outer query"));
        }
        let Some(state) = self.aggregates.as_mut() else {
            return Err(binder_error(self.aggregate_error));
        };
        if state.inside_aggregate {
            return Err(binder_error("aggregate function calls cannot be nested"));
        }
        if function.args.iter().any(|a| !a.name.is_empty()) {
            return Err(crate::unported("named function arguments"));
        }
        if function.export_state {
            return Err(crate::unported("EXPORT_STATE"));
        }
        state.bound_aggregate = true;
        state.inside_aggregate = true;
        let filter = match &function.filter {
            Some(filter) => Some(self.bind_expression(filter)?.cast_to(&LogicalType::BOOLEAN, false)),
            None => None,
        };
        let children = function
            .args
            .iter()
            .map(|a| self.bind_expression(&a.expr))
            .collect::<Result<Vec<_>>>()?;
        let set = BuiltinFunctions::get()
            .aggregate(&function.name.name)
            .expect("checked by the caller");
        let mut order_by = Vec::with_capacity(function.order_by.len());
        for node in &function.order_by {
            let expr = self.bind_expression(&node.expr)?;
            order_by.push((expr, self.options.resolve_order(node.order, node.nulls)));
        }
        let mut aggregate = bind_aggregate_function(set, children, filter, function.distinct)?;
        aggregate.order_by = order_by;
        let state = self.aggregates.as_mut().expect("still bound");
        state.inside_aggregate = false;
        // equal aggregates are computed once
        let key = format!(
            "{:?}{:?}{:?}{}{:?}",
            aggregate.function, aggregate.children, aggregate.filter, aggregate.distinct, aggregate.order_by
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
        Ok(BoundExpr::new(
            BoundExprKind::Between {
                input: Box::new(checked_cast(input, &target)?),
                lower: Box::new(checked_cast(lower, &target)?),
                upper: Box::new(checked_cast(upper, &target)?),
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
        let (mut bound, _) = self.resolve_coalesce_type(children, Some(name))?;
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
