// Ported from duckdb/src/planner/binder/expression/bind_{columnref,comparison,conjunction,
// constant,cast,function,operator}_expression.cpp, duckdb/src/parser/literal.cpp
// (NumberToValue), duckdb/src/function/scalar/comparison/comparison.cpp (ComparisonBind) and
// duckdb/src/catalog/default/default_types.cpp @ rust-port-baseline
//
//! Binding parsed expressions into [`BoundExpr`]s.

use super::{Binder, ColumnMode, binder_error};
use crate::execution::expression_executor::try_evaluate_scalar;
use crate::function::BuiltinFunctions;
use crate::function::binder::{bind_aggregate_function, bind_scalar_function, expression_return_type};
use crate::function::combine_types::{force_max_logical_type, normalize_type, try_max_logical_type};
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding};
use duckrs_common::types::{LogicalType, LogicalTypeId, decimal};
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    CastType, ComparisonOp, Expr, ExprKind, FunctionExpr, Literal, LiteralKind, OperatorType, TypeExpr,
};
use duckrs_parser::transformer::visit_expr_mut;

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
        return Err(crate::unported("BIGNUM literals"));
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
        LiteralKind::Bit => Err(crate::unported("BIT literals")),
    }
}

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

impl Binder<'_> {
    /// `Binder::BindLogicalType` for the types slice 1 executes: the scalar types without
    /// parameters, `VARCHAR(n)` and `DECIMAL(width, scale)`.
    pub(super) fn bind_type(&self, target: &CastType) -> Result<LogicalType> {
        let (name, children): (String, &[Expr]) = match target {
            CastType::Builtin(name) => (name.to_lowercase(), &[]),
            CastType::Parsed(expr) => match &expr.kind {
                ExprKind::Type(TypeExpr { name, children }) => {
                    if !name.qualification.is_empty() {
                        return Err(crate::unported("qualified type names"));
                    }
                    (name.name.to_lowercase(), children.as_slice())
                }
                _ => return Err(crate::unported(format!("type expression {expr}"))),
            },
        };
        let Some(id) = builtin_type(&name) else {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Type with name {name} does not exist!"),
            ));
        };
        match id {
            LogicalTypeId::Decimal => {
                let (width, scale) = match children {
                    [] => (18, 3),
                    [w] => (type_parameter(w).unwrap_or(-1), 0),
                    [w, s] => (type_parameter(w).unwrap_or(-1), type_parameter(s).unwrap_or(-1)),
                    _ => return Err(binder_error("DECIMAL type requires at most two type parameters")),
                };
                if !(1..=decimal::MAX_WIDTH as i64).contains(&width) {
                    return Err(binder_error(format!(
                        "Width must be between 1 and {}!",
                        decimal::MAX_WIDTH
                    )));
                }
                if !(0..=width).contains(&scale) {
                    return Err(binder_error("Scale must be between 0 and the width!"));
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
            _ if !children.is_empty() => Err(crate::unported(format!("type parameters for {}", name.to_uppercase()))),
            LogicalTypeId::Struct
            | LogicalTypeId::Tuple
            | LogicalTypeId::List
            | LogicalTypeId::Array
            | LogicalTypeId::Map
            | LogicalTypeId::Union
            | LogicalTypeId::Enum
            | LogicalTypeId::Variant
            | LogicalTypeId::Bit
            | LogicalTypeId::Bignum
            | LogicalTypeId::Geometry
            | LogicalTypeId::Type => Err(crate::unported(format!("the {} type", name.to_uppercase()))),
            id => Ok(LogicalType::new(id)),
        }
    }

    /// `ExpressionBinder::Bind`.
    pub(super) fn bind_expression(&mut self, expr: &Expr) -> Result<BoundExpr> {
        if let Some(group) = self.try_bind_group(expr) {
            return Ok(group);
        }
        let bound = match &expr.kind {
            ExprKind::Constant(literal) => BoundExpr::constant(literal_to_value(literal)?),
            ExprKind::ColumnRef(names) => {
                let mode = self
                    .aggregates
                    .as_ref()
                    .filter(|state| !state.inside_aggregate)
                    .map(|state| state.mode);
                if mode == Some(ColumnMode::Having) {
                    return self.bind_having_column(names);
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
                                return Err(error);
                            };
                            // an alias may not refer to itself
                            let (_, aliased) = self.alias_fallback.remove(position);
                            let bound = self.bind_expression(&aliased);
                            self.alias_fallback.insert(position, (column.clone(), aliased));
                            return bound;
                        }
                    },
                    [table, column] => self.bind_context.bind_qualified_column(table, column)?,
                    _ => return Err(crate::unported("column references with schema qualification")),
                };
                if mode == Some(ColumnMode::SelectList) {
                    self.aggregates.as_mut().unwrap().bound_columns.push(name.clone());
                }
                BoundExpr::column_ref(binding, ty, name)
            }
            ExprKind::Comparison { op, left, right } => {
                let left = self.bind_expression(left)?;
                let right = self.bind_expression(right)?;
                bind_comparison(*op, left, right)?
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
                if function.name.qualification.is_empty()
                    && BuiltinFunctions::get().aggregate(&function.name.name).is_some() =>
            {
                self.bind_aggregate(function)?
            }
            ExprKind::Function(function) => {
                if function.filter.is_some() || !function.order_by.is_empty() || function.distinct {
                    return Err(crate::unported("aggregate modifiers"));
                }
                if !function.name.qualification.is_empty() {
                    return Err(crate::unported("qualified function names"));
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
                let children = function
                    .args
                    .iter()
                    .map(|a| self.bind_expression(&a.expr))
                    .collect::<Result<Vec<_>>>()?;
                bind_scalar_function(set, children, function.is_operator, &try_evaluate_scalar)?
            }
            ExprKind::Star(_) => return Err(binder_error("STAR expression is not supported here")),
            other => {
                let name = format!("{other:?}");
                let kind: String = name.chars().take_while(|c| c.is_alphanumeric()).collect();
                return Err(crate::unported(format!("{kind} expressions")));
            }
        };
        Ok(bound)
    }
}

impl Binder<'_> {
    /// `BaseSelectBinder::TryBindGroup` + `BindGroup`: a reference to the group `expr` matches.
    fn try_bind_group(&self, expr: &Expr) -> Option<BoundExpr> {
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
                "column {column} must appear in the GROUP BY clause or be used in an aggregate function"
            )));
        }
        let group_index = state.group_index;
        let (binding, ty, name) = match names {
            [column] => self.bind_context.bind_column(column)?,
            [table, column] => self.bind_context.bind_qualified_column(table, column)?,
            _ => return Err(crate::unported("column references with schema qualification")),
        };
        let state = self.aggregates.as_mut().expect("HAVING binds with a grouping");
        let group = state.groups.len();
        state
            .groups
            .push(BoundExpr::column_ref(binding, ty.clone(), name.clone()));
        Ok(BoundExpr::column_ref(ColumnBinding::new(group_index, group), ty, name))
    }

    /// `BaseSelectBinder::BindAggregate`.
    fn bind_aggregate(&mut self, function: &FunctionExpr) -> Result<BoundExpr> {
        let Some(state) = self.aggregates.as_mut() else {
            return Err(binder_error(self.aggregate_error));
        };
        if state.inside_aggregate {
            return Err(binder_error("aggregate function calls cannot be nested"));
        }
        if !function.order_by.is_empty() {
            return Err(crate::unported("ORDER BY in aggregates"));
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
        for child in &children {
            if !supported_aggregate_input(&child.return_type) {
                return Err(crate::unported(format!("aggregates over {}", child.return_type)));
            }
        }
        let aggregate = bind_aggregate_function(set, children, filter, function.distinct)?;
        let state = self.aggregates.as_mut().expect("still bound");
        state.inside_aggregate = false;
        // equal aggregates are computed once
        let key = format!(
            "{:?}{:?}{:?}{}",
            aggregate.function, aggregate.children, aggregate.filter, aggregate.distinct
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

/// The nested types have no vector support in the aggregate states yet.
fn supported_aggregate_input(ty: &LogicalType) -> bool {
    !ty.is_nested()
}

/// An expression with all aliases removed: `ParsedExpression::Equals` ignores aliases.
pub(super) fn without_aliases(expr: &Expr) -> Expr {
    let mut expr = expr.clone();
    visit_expr_mut(&mut expr, &mut |e| e.alias.clear());
    expr
}

/// `ComparisonBind` + `BoundComparisonExpression::TryBindComparison`: both sides are cast to
/// the type they can be compared in.
fn bind_comparison(op: ComparisonOp, left: BoundExpr, right: BoundExpr) -> Result<BoundExpr> {
    let left_type = expression_return_type(&left);
    let right_type = expression_return_type(&right);
    let is_equality = matches!(
        op,
        ComparisonOp::Equal | ComparisonOp::NotEqual | ComparisonOp::DistinctFrom | ComparisonOp::NotDistinctFrom
    );
    let mut input_type = if is_equality {
        force_max_logical_type(&left_type, &right_type)
    } else {
        try_max_logical_type(&left_type, &right_type).ok_or_else(|| {
            binder_error(format!(
                "Cannot compare values of type {left_type} and type {right_type} - an explicit cast is required"
            ))
        })?
    };
    match input_type.id() {
        LogicalTypeId::Decimal => {
            // the maximum width and the maximum scale over width
            let props = [&left_type, &right_type].map(crate::function::combine_types::decimal_properties);
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
            if left_type.id() != LogicalTypeId::Varchar && switch_varchar_comparison(&left_type) {
                input_type = normalize_type(&left_type);
            } else if right_type.id() != LogicalTypeId::Varchar && switch_varchar_comparison(&right_type) {
                input_type = normalize_type(&right_type);
            }
        }
        _ => {}
    }
    let input_type = normalize_type(&input_type);
    for side in [&left, &right] {
        if !crate::function::cast::has_cast(&side.return_type, &input_type) {
            return Err(crate::unported(format!("casting {} to {input_type}", side.return_type)));
        }
    }
    Ok(BoundExpr::new(
        BoundExprKind::Comparison {
            op,
            left: Box::new(left.cast_to(&input_type, false)),
            right: Box::new(right.cast_to(&input_type, false)),
        },
        LogicalType::BOOLEAN,
    ))
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
