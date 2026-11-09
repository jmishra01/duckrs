//! Ported from duckdb/src/parser/qualified_name.cpp and duckdb/src/parser/expression/
//! {cast,function,lambda,window}_expression.cpp @ rust-port-baseline.
//!
//! The classes that are marked `custom_implementation` in the specs, as they read fields of older
//! versions that the specs don't list.

use super::generated::enums::lambda_syntax_type;
use super::generated::{FunctionArgument, OrderByNode, ParsedExpression, ResultModifier};
use duckrs_common::Result;
use duckrs_common::serializer::{BinaryDeserializer, BinarySerializer, Deserialize, Serialize, StorageVersion};
use duckrs_common::types::LogicalType;

type Child = Option<Box<ParsedExpression>>;

#[derive(Debug, Clone, Default)]
pub struct QualifiedName {
    pub path: Vec<String>,
}

impl Deserialize for QualifiedName {
    fn read_value(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        d.read_object_value(|d| {
            Ok(Self {
                path: d.read_property_with_default(100, "path")?,
            })
        })
    }
}

impl Serialize for QualifiedName {
    fn write_value(&self, s: &mut BinarySerializer) {
        s.write_object_value(|s| s.write_property_with_default(100, "path", &self.path));
    }
}

/// The target of a CAST: a parsed type, or a `LogicalType` in files from before v2.0.0.
#[derive(Debug, Clone)]
pub struct CastExpression {
    pub child: Child,
    pub cast_type: Option<LogicalType>,
    pub try_cast: bool,
    pub type_expr: Child,
}

impl CastExpression {
    pub fn read_members(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        Ok(Self {
            child: d.read_property_with_default(200, "child")?,
            cast_type: d.read_optional_property(201, "cast_type")?,
            try_cast: d.read_property_with_default(202, "try_cast")?,
            type_expr: d.read_property_with_default(203, "type_expr")?,
        })
    }

    pub fn write_members(&self, s: &mut BinarySerializer) {
        s.write_property_with_default(200, "child", &self.child);
        s.write_property_with_default(202, "try_cast", &self.try_cast);
        s.write_property_with_default(203, "type_expr", &self.type_expr);
    }
}

/// The spec of a lambda is complete, but what it says of its syntax depends on the file.
#[derive(Debug, Clone)]
pub struct LambdaExpression {
    pub lhs: Child,
    pub expr: Child,
    pub syntax_type: u64,
}

impl LambdaExpression {
    /// `LambdaExpression::FinalizeDeserialization`: arrow lambdas stored before v2.0.0 keep binding.
    pub fn read_members(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let lhs = d.read_property_with_default(200, "lhs")?;
        let expr = d.read_property_with_default(201, "expr")?;
        let mut syntax_type = d
            .read_optional_property(202, "syntax_type")?
            .unwrap_or(lambda_syntax_type::SINGLE_ARROW_STORAGE);
        if syntax_type == lambda_syntax_type::SINGLE_ARROW && d.is_from_before_v2() {
            syntax_type = lambda_syntax_type::SINGLE_ARROW_STORAGE;
        }
        Ok(Self { lhs, expr, syntax_type })
    }

    pub fn write_members(&self, s: &mut BinarySerializer) {
        s.write_property_with_default(200, "lhs", &self.lhs);
        s.write_property_with_default(201, "expr", &self.expr);
        if self.syntax_type != lambda_syntax_type::SINGLE_ARROW_STORAGE {
            s.write_property(202, "syntax_type", &self.syntax_type);
        }
    }
}

#[derive(Debug, Clone)]
pub struct FunctionExpression {
    pub function_name: String,
    pub schema: String,
    /// Arguments before v2.0.0, which have their names as aliases.
    pub children: Vec<Child>,
    pub filter: Child,
    pub order_bys: Option<Box<ResultModifier>>,
    pub distinct: bool,
    pub is_operator: bool,
    pub export_state: bool,
    pub catalog: String,
    pub arguments: Vec<FunctionArgument>,
    pub qualified_name: Option<QualifiedName>,
}

impl FunctionExpression {
    pub fn read_members(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let result = Self {
            function_name: d.read_property_with_default(200, "function_name")?,
            schema: d.read_property_with_default(201, "schema")?,
            children: d.read_property_with_default(202, "children")?,
            filter: d.read_property_with_default(203, "filter")?,
            order_bys: d.read_property_with_default(204, "order_bys")?,
            distinct: d.read_property_with_default(205, "distinct")?,
            is_operator: d.read_property_with_default(206, "is_operator")?,
            export_state: d.read_property_with_default(207, "export_state")?,
            catalog: d.read_property_with_default(208, "catalog")?,
            arguments: d.read_property_with_default(209, "arguments")?,
            qualified_name: d.read_optional_property(210, "qualified_name")?,
        };
        d.read_property_with_default::<bool>(211, "is_legacy_function_call")?;
        Ok(result)
    }

    pub fn write_members(&self, s: &mut BinarySerializer) {
        s.write_property_with_default(200, "function_name", &self.function_name);
        s.write_property_with_default(201, "schema", &self.schema);
        if !s.should_serialize(StorageVersion::V2_0_0) {
            s.write_property_with_default(202, "children", &legacy_children(&self.arguments));
        }
        s.write_property_with_default(203, "filter", &self.filter);
        s.write_property_with_default(204, "order_bys", &self.order_bys);
        s.write_property_with_default(205, "distinct", &self.distinct);
        s.write_property_with_default(206, "is_operator", &self.is_operator);
        s.write_property_with_default(207, "export_state", &self.export_state);
        s.write_property_with_default(208, "catalog", &self.catalog);
        if s.should_serialize(StorageVersion::V2_0_0) {
            s.write_property_with_default(209, "arguments", &self.arguments);
        }
    }
}

#[derive(Debug, Clone)]
pub struct WindowExpression {
    pub function_name: String,
    pub schema: String,
    pub catalog: String,
    pub children: Vec<Child>,
    pub partitions: Vec<Child>,
    pub orders: Vec<OrderByNode>,
    pub start: u64,
    pub end: u64,
    pub start_expr: Child,
    pub end_expr: Child,
    /// The second and third arguments of `lead` and `lag` before v2.0.0. C++ drops them on reading,
    /// which turns `lag(x, 2, 0)` into `lag(x)`; here they stay arguments.
    pub offset_expr: Child,
    pub default_expr: Child,
    pub ignore_nulls: bool,
    pub filter_expr: Child,
    pub exclude_clause: Option<u64>,
    pub distinct: bool,
    pub arg_orders: Vec<OrderByNode>,
    pub has_ignore_nulls: bool,
    pub arguments: Vec<FunctionArgument>,
    pub qualified_name: Option<QualifiedName>,
}

impl WindowExpression {
    pub fn read_members(d: &mut BinaryDeserializer<'_>) -> Result<Self> {
        let result = Self {
            function_name: d.read_property_with_default(200, "function_name")?,
            schema: d.read_property_with_default(201, "schema")?,
            catalog: d.read_property_with_default(202, "catalog")?,
            children: d.read_property_with_default(203, "children")?,
            partitions: d.read_property_with_default(204, "partitions")?,
            orders: d.read_property_with_default(205, "orders")?,
            start: d.read_property(206, "start")?,
            end: d.read_property(207, "end")?,
            start_expr: d.read_property_with_default(208, "start_expr")?,
            end_expr: d.read_property_with_default(209, "end_expr")?,
            offset_expr: d.read_property_with_default(210, "offset_expr")?,
            default_expr: d.read_property_with_default(211, "default_expr")?,
            ignore_nulls: d.read_property_with_default(212, "ignore_nulls")?,
            filter_expr: d.read_property_with_default(213, "filter_expr")?,
            exclude_clause: d.read_optional_property(214, "exclude_clause")?,
            distinct: d.read_property_with_default(215, "distinct")?,
            arg_orders: d.read_property_with_default(216, "arg_orders")?,
            has_ignore_nulls: d.read_property_with_default(217, "has_ignore_nulls")?,
            arguments: d.read_property_with_default(218, "arguments")?,
            qualified_name: d.read_optional_property(219, "qualified_name")?,
        };
        d.read_property_with_default::<bool>(220, "is_legacy_function_call")?;
        Ok(result)
    }

    pub fn write_members(&self, s: &mut BinarySerializer) {
        s.write_property_with_default(200, "function_name", &self.function_name);
        s.write_property_with_default(201, "schema", &self.schema);
        s.write_property_with_default(202, "catalog", &self.catalog);
        let legacy = !s.should_serialize(StorageVersion::V2_0_0);
        if legacy {
            s.write_property_with_default(203, "children", &legacy_children(&self.arguments));
        }
        s.write_property_with_default(204, "partitions", &self.partitions);
        s.write_property_with_default(205, "orders", &self.orders);
        s.write_property(206, "start", &self.start);
        s.write_property(207, "end", &self.end);
        s.write_property_with_default(208, "start_expr", &self.start_expr);
        s.write_property_with_default(209, "end_expr", &self.end_expr);
        // `SerializedOffset` and `SerializedDefault`: lead and lag have them apart before v2.0.0
        let lead_or_lag = matches!(self.function_name.as_str(), "lead" | "lag");
        let argument = |at: usize| -> Child {
            if legacy && lead_or_lag {
                self.arguments.get(at).and_then(|argument| argument.expression.clone())
            } else {
                None
            }
        };
        s.write_property_with_default(210, "offset_expr", &argument(1));
        s.write_property_with_default(211, "default_expr", &argument(2));
        s.write_property_with_default(212, "ignore_nulls", &self.ignore_nulls);
        s.write_property_with_default(213, "filter_expr", &self.filter_expr);
        if let Some(exclude) = &self.exclude_clause {
            s.write_property(214, "exclude_clause", exclude);
        }
        s.write_property_with_default(215, "distinct", &self.distinct);
        s.write_property_with_default(216, "arg_orders", &self.arg_orders);
        if !legacy {
            s.write_property_with_default(217, "has_ignore_nulls", &self.has_ignore_nulls);
            s.write_property_with_default(218, "arguments", &self.arguments);
        }
    }
}

/// The arguments of a function as files before v2.0.0 have them: expressions, named by their aliases.
fn legacy_children(arguments: &[FunctionArgument]) -> Vec<Child> {
    arguments
        .iter()
        .map(|argument| {
            argument.expression.clone().map(|mut expression| {
                if !argument.name.is_empty() {
                    expression.alias.clone_from(&argument.name);
                }
                expression
            })
        })
        .collect()
}
