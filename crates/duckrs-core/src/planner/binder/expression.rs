// Ported from duckdb/src/planner/binder/expression/bind_{columnref,comparison,conjunction,
// constant,cast,function,operator}_expression.cpp, duckdb/src/parser/literal.cpp
// (NumberToValue), duckdb/src/function/scalar/comparison/comparison.cpp (ComparisonBind) and
// duckdb/src/catalog/default/default_types.cpp @ rust-port-baseline
//
//! Binding parsed expressions into [`BoundExpr`]s.

use super::{Binder, ColumnMode, binder_error};
use crate::catalog::DEFAULT_CATALOG;
use crate::execution::expression_executor::{evaluate_scalar, try_evaluate_scalar};
use crate::function::BuiltinFunctions;
use crate::function::binder::{bind_aggregate_function, bind_scalar_function, expression_return_type};
use crate::function::combine_types::{force_max_logical_type, normalize_type, try_max_logical_type};
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator, ColumnBinding, SequenceOp};
use duckrs_common::types::{LogicalType, LogicalTypeId, decimal};
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::sql_quoted_identifier;
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
        if let ExprKind::Function(function) = &expr.kind
            && let Some(entry) = self.find_macro(&function.name, false)?
        {
            return self.bind_macro(function, &entry);
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
                let bound = bind_scalar_function(set, children, function.is_operator, &try_evaluate_scalar)?;
                self.bind_sequence_function(bound)?
            }
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

    /// `BaseSelectBinder::BindAggregate`.
    fn bind_aggregate(&mut self, function: &FunctionExpr) -> Result<BoundExpr> {
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
        for child in &children {
            if !supported_aggregate_input(&child.return_type) {
                return Err(crate::unported(format!("aggregates over {}", child.return_type)));
            }
        }
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
