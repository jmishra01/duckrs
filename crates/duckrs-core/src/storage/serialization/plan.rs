// Ported from duckdb/src/storage/serialization/serialize_logical_operator.cpp,
// serialize_expression.cpp, serialize_nodes.cpp, duckdb/src/planner/operator/logical_get.cpp,
// logical_column_data_get.cpp, logical_copy_to_file.cpp and
// duckdb/src/include/duckdb/function/function_serialization.hpp @ rust-port-baseline
//
//! What `json_serialize_plan` writes: a logical plan in the form of DuckDB's `LogicalOperator::Serialize`, with
//! the bound expressions of `Expression::Serialize` and the functions of `FunctionSerializer`, as JSON.
//!
//! duckrs's operators differ from DuckDB's classes, so each is written as the DuckDB operator it stands for:
//! - `Get` (a base table), `Range`, `CsvScan`, `JsonScan`, `ParquetScan`: `LOGICAL_GET` of `seq_scan`, `range` or
//!   `generate_series`, and the file readers. Only `seq_scan` writes its bind data (`TableScanSerialize`); the
//!   readers write their name, files and columns, not their options.
//! - `ChunkGet`, and an `ExpressionGet` of rows known while binding (SHOW, DESCRIBE): `LOGICAL_CHUNK_GET`.
//! - `ColumnSelect` has no operator of its own in DuckDB (it is a projection map there): its child stands in for it.
//! - `Join` is `LOGICAL_COMPARISON_JOIN` with conditions and `LOGICAL_ANY_JOIN` without; a residual predicate
//!   next to conditions is written under the field that held it before DuckDB split it into conditions.
//! - `MarkJoin` is a join of type MARK, `WorkingTable` a `LOGICAL_CTE_REF` of its recursive CTE.
//! - casts, comparisons and BETWEEN are function expressions (`__cast`, `=`, `__between`), as in DuckDB now.
//!
//! `LateralJoin`, `MatchRecognize` and subquery expressions (DuckDB plans none of them) are errors, like an
//! operator that does not support serialization.

use super::generated as g;
use super::generated::enums::catalog_type;
use super::to_stored;
use crate::catalog::{ColumnDefinition, LogicalColumn, TableCatalogEntry};
use crate::planner::expression::{
    BoundAggregate, BoundExpr, BoundExprKind, BoundOperator, BoundWindow, LambdaKind, SequenceOp, WindowFunction,
};
use crate::planner::logical::{
    BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID, COLUMN_IDENTIFIER_ROW_NUMBER, JoinCondition, LogicalOperator,
    LogicalOperatorKind as K,
};
use duckrs_common::json::Json;
use duckrs_common::serializer::{BinarySerializer, SerializationOptions, Serialize, StorageVersion};
use duckrs_common::sort_key::{NullOrder, OrderModifiers, OrderType};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{self, ComparisonOp, JoinType, MergeActionType, MergeCondition, WindowBoundary};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

/// What `JsonSerializer::Serialize` leaves out.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanJsonOptions {
    pub skip_null: bool,
    pub skip_empty: bool,
    pub skip_default: bool,
}

impl PlanJsonOptions {
    /// Plan inspection writes the complete current representation (`StorageCompatibility::Latest`).
    fn serialization(self) -> SerializationOptions {
        SerializationOptions {
            serialize_default_values: !self.skip_default,
            target: StorageVersion::LATEST,
        }
    }
}

/// The columns of each `LOGICAL_CHUNK_GET` (by table index) that the plan above it reads, which DuckDB's
/// `RemoveUnusedColumns` leaves as its `column_ids`. The plan must not have its bindings resolved yet.
pub fn chunk_get_columns(plan: &mut LogicalOperator) -> HashMap<usize, Vec<usize>> {
    let mut chunks = HashMap::new();
    collect_chunk_gets(plan, &mut chunks);
    let mut used: HashMap<usize, BTreeSet<usize>> = HashMap::new();
    collect_used_columns(plan, &chunks, &mut used);
    chunks
        .into_iter()
        .map(|(table_index, count)| {
            let columns = used.remove(&table_index).unwrap_or_default();
            // a scan of none of them still reads one, as `RemoveUnusedColumns` keeps the first
            let columns = if columns.is_empty() {
                (0..count.min(1)).collect()
            } else {
                columns.into_iter().collect()
            };
            (table_index, columns)
        })
        .collect()
}

fn collect_chunk_gets(op: &LogicalOperator, chunks: &mut HashMap<usize, usize>) {
    match &op.kind {
        K::ChunkGet { table_index, types, .. } => {
            chunks.insert(*table_index, types.len());
        }
        K::ExpressionGet {
            table_index,
            types,
            materialized: true,
            ..
        } => {
            chunks.insert(*table_index, types.len());
        }
        K::RecursiveCte { recursive, .. } => collect_chunk_gets(recursive, chunks),
        _ => {}
    }
    for child in &op.children {
        collect_chunk_gets(child, chunks);
    }
}

fn collect_used_columns(
    op: &mut LogicalOperator,
    chunks: &HashMap<usize, usize>,
    used: &mut HashMap<usize, BTreeSet<usize>>,
) {
    for expr in op.expressions_mut() {
        expr.walk_mut(&mut |e| {
            if let BoundExprKind::ColumnRef { binding, .. } = &e.kind
                && chunks.contains_key(&binding.table_index)
            {
                used.entry(binding.table_index)
                    .or_default()
                    .insert(binding.column_index);
            }
        });
    }
    // what leaves the plan is read too
    if let K::RecursiveCte { recursive, .. } = &mut op.kind {
        collect_used_columns(recursive, chunks, used);
    }
    for child in &mut op.children {
        collect_used_columns(child, chunks, used);
    }
}

/// `LogicalOperator::Serialize` of `plan`, whose bindings are resolved, as JSON.
pub fn serialize_plan(
    plan: &LogicalOperator,
    chunk_columns: HashMap<usize, Vec<usize>>,
    options: PlanJsonOptions,
) -> Result<Json> {
    let mut ctes = HashMap::new();
    collect_working_tables(plan, &mut ctes);
    let writer = PlanWriter {
        chunk_columns,
        ctes,
        error: RefCell::new(None),
    };
    let json = BinarySerializer::serialize_json(
        &W(|s: &mut BinarySerializer| writer.op(s, plan), false),
        options.serialization(),
        options.skip_null,
        options.skip_empty,
    );
    match writer.error.into_inner() {
        Some(error) => Err(error),
        None => Ok(json),
    }
}

/// `LogicalCreate::Serialize` of a CREATE MACRO (`LOGICAL_CREATE_MACRO`), whose types `bind` resolves.
pub fn serialize_create_macro(
    info: &ast::CreateInfo,
    bind: &dyn Fn(&ast::CastType) -> Result<LogicalType>,
    options: PlanJsonOptions,
) -> Result<Json> {
    let ast::CreateKind::Macro { table_macro, macros } = &info.kind else {
        return Err(Error::new(ErrorKind::Internal, "not a CREATE MACRO"));
    };
    let mut functions = macros.iter().map(|function| to_stored::macro_function(function, bind));
    let function = functions
        .next()
        .ok_or_else(|| Error::new(ErrorKind::Internal, "a macro has no function"))??;
    let extra_functions = functions
        .map(|function| function.map(|f| Some(Box::new(f))))
        .collect::<Result<_>>()?;
    let mut path = info.name.qualification.clone();
    if path.is_empty() {
        path.push(crate::catalog::DEFAULT_SCHEMA.to_string());
    }
    path.push(info.name.name.clone());
    let mut stored = to_stored::create_info(
        if *table_macro {
            catalog_type::TABLE_MACRO_ENTRY
        } else {
            catalog_type::MACRO_ENTRY
        },
        path,
        g::CreateInfoKind::CreateMacroInfo(g::CreateMacroInfo {
            name: info.name.name.clone(),
            function: Some(Box::new(function)),
            extra_functions,
        }),
    );
    stored.temporary = info.temporary;
    stored.on_conflict = on_create_conflict(info.on_conflict);
    stored.sql = info.to_string();
    Ok(BinarySerializer::serialize_json(
        &W(
            |s: &mut BinarySerializer| {
                s.write_object_value(|s| {
                    s.write_property(100, "type", "LOGICAL_CREATE_MACRO");
                    s.write_property_with_default(101, "children", &Vec::<u64>::new());
                    s.write_property_with_default(200, "info", &W(|s| ptr(s, &stored), false));
                })
            },
            false,
        ),
        options.serialization(),
        options.skip_null,
        options.skip_empty,
    ))
}

fn on_create_conflict(conflict: ast::OnCreateConflict) -> u64 {
    use g::enums::on_create_conflict::*;
    match conflict {
        ast::OnCreateConflict::Error => ERROR_ON_CONFLICT,
        ast::OnCreateConflict::Ignore => IGNORE_ON_CONFLICT,
        ast::OnCreateConflict::Replace => REPLACE_ON_CONFLICT,
    }
}

/// The recursive CTE each working table belongs to, by the address of the rows they share.
fn collect_working_tables(op: &LogicalOperator, ctes: &mut HashMap<usize, usize>) {
    if let K::RecursiveCte {
        working,
        table_index,
        recursive,
        ..
    } = &op.kind
    {
        ctes.insert(std::sync::Arc::as_ptr(working) as usize, *table_index);
        collect_working_tables(recursive, ctes);
    }
    for child in &op.children {
        collect_working_tables(child, ctes);
    }
}

/// What writes one value.
type Writer<'a> = Box<dyn Fn(&mut BinarySerializer) + 'a>;

/// A value that writes itself with a closure, and whether it is its type's default.
struct W<F: Fn(&mut BinarySerializer)>(F, bool);

impl<F: Fn(&mut BinarySerializer)> Serialize for W<F> {
    fn write_value(&self, s: &mut BinarySerializer) {
        (self.0)(s)
    }
    fn is_default(&self) -> bool {
        self.1
    }
}

/// A `unique_ptr` that is there: the presence flag, then the value.
fn ptr<T: Serialize + ?Sized>(s: &mut BinarySerializer, value: &T) {
    s.begin_nullable(true);
    value.write_value(s);
}

/// The list `items`, each written by `each`.
fn list<T>(s: &mut BinarySerializer, items: &[T], mut each: impl FnMut(&mut BinarySerializer, &T)) {
    s.begin_list(items.len());
    for item in items {
        each(s, item);
    }
    s.end_list();
}

fn u64s(items: impl IntoIterator<Item = usize>) -> Vec<u64> {
    items.into_iter().map(|i| i as u64).collect()
}

/// `QualifiedName::Serialize`.
fn qualified_name(s: &mut BinarySerializer, path: &[String]) {
    s.write_object_value(|s| s.write_property_with_default(100, "path", &path.to_vec()));
}

/// `ColumnBinding::Serialize`.
fn column_binding(s: &mut BinarySerializer, table_index: usize, column_index: usize) {
    s.write_object_value(|s| {
        s.write_property_with_default(100, "table_index", &(table_index as u64));
        s.write_property_with_default(101, "column_index", &(column_index as u64));
    });
}

/// `optional_idx`, which is written as the index, `DConstants::INVALID_INDEX` without one.
const INVALID_INDEX: u64 = u64::MAX;

fn join_type_name(join_type: JoinType) -> &'static str {
    match join_type {
        JoinType::Inner => "INNER",
        JoinType::Left => "LEFT",
        JoinType::Right => "RIGHT",
        JoinType::Outer => "OUTER",
        JoinType::Semi => "SEMI",
        JoinType::Anti => "ANTI",
        JoinType::Mark => "MARK",
        JoinType::Single => "SINGLE",
        JoinType::RightSemi => "RIGHT_SEMI",
        JoinType::RightAnti => "RIGHT_ANTI",
    }
}

/// The `ExpressionType` of a comparison and the name of its function (`GetComparisonFunction`).
fn comparison_names(op: ComparisonOp) -> (&'static str, &'static str) {
    match op {
        ComparisonOp::Equal => ("COMPARE_EQUAL", "="),
        ComparisonOp::NotEqual => ("COMPARE_NOTEQUAL", "!="),
        ComparisonOp::LessThan => ("COMPARE_LESSTHAN", "<"),
        ComparisonOp::GreaterThan => ("COMPARE_GREATERTHAN", ">"),
        ComparisonOp::LessThanOrEqual => ("COMPARE_LESSTHANOREQUALTO", "<="),
        ComparisonOp::GreaterThanOrEqual => ("COMPARE_GREATERTHANOREQUALTO", ">="),
        ComparisonOp::DistinctFrom => ("COMPARE_DISTINCT_FROM", "IS DISTINCT FROM"),
        ComparisonOp::NotDistinctFrom => ("COMPARE_NOT_DISTINCT_FROM", "IS NOT DISTINCT FROM"),
    }
}

fn window_boundary_name(boundary: WindowBoundary) -> &'static str {
    match boundary {
        WindowBoundary::Invalid => "INVALID",
        WindowBoundary::UnboundedPreceding => "UNBOUNDED_PRECEDING",
        WindowBoundary::UnboundedFollowing => "UNBOUNDED_FOLLOWING",
        WindowBoundary::CurrentRowRange => "CURRENT_ROW_RANGE",
        WindowBoundary::CurrentRowRows => "CURRENT_ROW_ROWS",
        WindowBoundary::CurrentRowGroups => "CURRENT_ROW_GROUPS",
        WindowBoundary::ExprPrecedingRows => "EXPR_PRECEDING_ROWS",
        WindowBoundary::ExprPrecedingRange => "EXPR_PRECEDING_RANGE",
        WindowBoundary::ExprPrecedingGroups => "EXPR_PRECEDING_GROUPS",
        WindowBoundary::ExprFollowingRows => "EXPR_FOLLOWING_ROWS",
        WindowBoundary::ExprFollowingRange => "EXPR_FOLLOWING_RANGE",
        WindowBoundary::ExprFollowingGroups => "EXPR_FOLLOWING_GROUPS",
    }
}

fn window_type_name(function: WindowFunction) -> &'static str {
    match function {
        WindowFunction::Aggregate => "WINDOW_AGGREGATE",
        WindowFunction::RowNumber => "WINDOW_ROW_NUMBER",
        WindowFunction::Rank => "WINDOW_RANK",
        WindowFunction::DenseRank => "WINDOW_RANK_DENSE",
        WindowFunction::PercentRank => "WINDOW_PERCENT_RANK",
        WindowFunction::CumeDist => "WINDOW_CUME_DIST",
        WindowFunction::Ntile => "WINDOW_NTILE",
        WindowFunction::Lag => "WINDOW_LAG",
        WindowFunction::Lead => "WINDOW_LEAD",
        WindowFunction::FirstValue => "WINDOW_FIRST_VALUE",
        WindowFunction::LastValue => "WINDOW_LAST_VALUE",
        WindowFunction::NthValue => "WINDOW_NTH_VALUE",
        WindowFunction::Fill => "WINDOW_FILL",
    }
}

fn operator_type_name(op: BoundOperator) -> &'static str {
    match op {
        BoundOperator::Not => "OPERATOR_NOT",
        BoundOperator::IsNull => "OPERATOR_IS_NULL",
        BoundOperator::IsNotNull => "OPERATOR_IS_NOT_NULL",
        BoundOperator::Coalesce => "OPERATOR_COALESCE",
        BoundOperator::Try => "OPERATOR_TRY",
        BoundOperator::HasError => "OPERATOR_HAS_ERROR",
        BoundOperator::In => "COMPARE_IN",
        BoundOperator::NotIn => "COMPARE_NOT_IN",
    }
}

/// The function a call names: what `FunctionSerializer::Serialize` writes of it.
struct FunctionInfo<'a> {
    name: &'a str,
    arguments: &'a [LogicalType],
    /// `BoundSimpleFunction::GetNamedArguments`: the arguments given by name, after the positional ones
    named: Vec<String>,
    /// what the serialize callback writes as `function_data`, if the function has one
    data: Option<&'a dyn Fn(&mut BinarySerializer)>,
}

/// The names a call of `struct_pack` or `union_value` gives its arguments, which DuckDB keeps as named
/// arguments (`SetCaptureArgumentAliases`); every other function is called by position.
fn named_arguments(name: &str, return_type: &LogicalType, count: usize) -> Vec<String> {
    let children = return_type.struct_children();
    let names: Vec<String> = match (name.to_ascii_lowercase().as_str(), return_type.id()) {
        ("struct_pack", LogicalTypeId::Struct) => children.iter().map(|(n, _)| n.clone()).collect(),
        // a UNION's children start with its tag
        ("union_value", LogicalTypeId::Union) => children.iter().skip(1).map(|(n, _)| n.clone()).collect(),
        _ => return Vec::new(),
    };
    if names.len() == count { names } else { Vec::new() }
}

struct PlanWriter {
    chunk_columns: HashMap<usize, Vec<usize>>,
    ctes: HashMap<usize, usize>,
    /// the first thing that could not be written
    error: RefCell<Option<Error>>,
}

impl PlanWriter {
    fn fail(&self, error: Error) {
        let mut slot = self.error.borrow_mut();
        if slot.is_none() {
            *slot = Some(error);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // expressions
    // ---------------------------------------------------------------------------------------------

    /// `Expression::Serialize` of `expr`, as an object.
    fn expr(&self, s: &mut BinarySerializer, expr: &BoundExpr) {
        s.write_object_value(|s| self.expr_body(s, expr));
    }

    /// `expr` as a `unique_ptr<Expression>` element.
    fn expr_ptr(&self, s: &mut BinarySerializer, expr: &BoundExpr) {
        s.begin_nullable(true);
        self.expr(s, expr);
    }

    /// A `vector<unique_ptr<Expression>>` property, left out when empty and defaults are.
    fn exprs_property(&self, s: &mut BinarySerializer, field: u16, tag: &str, exprs: &[BoundExpr]) {
        s.write_property_with_default(
            field,
            tag,
            &W(|s| list(s, exprs, |s, e| self.expr_ptr(s, e)), exprs.is_empty()),
        );
    }

    /// A `unique_ptr<Expression>` property with a null default.
    fn optional_expr_property(&self, s: &mut BinarySerializer, field: u16, tag: &str, expr: Option<&BoundExpr>) {
        s.write_property_with_default(
            field,
            tag,
            &W(
                |s| match expr {
                    Some(e) => self.expr_ptr(s, e),
                    None => s.begin_nullable(false),
                },
                expr.is_none(),
            ),
        );
    }

    /// The fields of `Expression::Serialize`. duckrs keeps no query locations.
    fn header(s: &mut BinarySerializer, class: &str, ty: &str, alias: &str) {
        s.write_property(100, "expression_class", class);
        s.write_property(101, "type", ty);
        s.write_property_with_default(102, "alias", alias);
        s.write_property_with_explicit_default(103, "query_location", &INVALID_INDEX, &INVALID_INDEX);
    }

    /// `FunctionSerializer::Serialize` at the current storage version.
    fn function(s: &mut BinarySerializer, function: &FunctionInfo<'_>, positional: usize) {
        if !s.should_serialize(StorageVersion::V2_0_0) {
            s.write_property(500, "name", function.name);
        }
        s.write_property(501, "arguments", &function.arguments.to_vec());
        if !s.should_serialize(StorageVersion::V2_0_0) {
            s.write_property(502, "original_arguments", &Vec::<LogicalType>::new());
            s.write_property_with_default(505, "catalog_name", "system");
            s.write_property_with_default(506, "schema_name", "main");
        } else {
            let path = ["system".to_string(), "main".to_string(), function.name.to_string()];
            s.write_property(507, "qname", &W(|s| qualified_name(s, &path), false));
        }
        s.write_property(503, "has_serialize", &function.data.is_some());
        if s.should_serialize(StorageVersion::V2_0_0) {
            s.write_property(508, "positional_arguments", &(positional as u64));
            s.write_property_with_default(509, "named_arguments", &function.named);
        }
        if let Some(data) = function.data {
            s.write_object(504, "function_data", data);
        }
    }

    /// `BoundFunctionExpression::Serialize`.
    #[allow(clippy::too_many_arguments, reason = "the fields of a function expression")]
    fn function_expr(
        &self,
        s: &mut BinarySerializer,
        ty: &str,
        alias: &str,
        return_type: &LogicalType,
        children: &[&dyn Fn(&mut BinarySerializer)],
        function: &FunctionInfo<'_>,
        is_operator: bool,
    ) {
        Self::header(s, "BOUND_FUNCTION", ty, alias);
        s.write_property(200, "return_type", return_type);
        s.write_property(
            201,
            "children",
            &W(
                |s| {
                    s.begin_list(children.len());
                    for child in children {
                        s.begin_nullable(true);
                        child(s);
                    }
                    s.end_list();
                },
                children.is_empty(),
            ),
        );
        let positional = children.len() - function.named.len();
        Self::function(s, function, positional);
        s.write_property(202, "is_operator", &is_operator);
    }

    fn expr_body(&self, s: &mut BinarySerializer, expr: &BoundExpr) {
        let alias = expr.alias.as_str();
        let return_type = &expr.return_type;
        match &expr.kind {
            // the filters that joins and Top-Ns give scans are not part of a DuckDB plan
            BoundExprKind::JoinFilter { child, .. } | BoundExprKind::TopNFilter { child, .. } => {
                self.expr_body(s, child)
            }
            BoundExprKind::Constant(value) => {
                Self::header(s, "BOUND_CONSTANT", "VALUE_CONSTANT", alias);
                s.write_property(200, "value", value);
            }
            // only a statement being prepared has one, and its plan is not serialized: as the NULL it evaluates to
            BoundExprKind::Parameter { .. } => {
                Self::header(s, "BOUND_CONSTANT", "VALUE_CONSTANT", alias);
                s.write_property(200, "value", &crate::planner::expression::parameter_null(return_type));
            }
            BoundExprKind::ColumnRef { binding, depth } => {
                Self::header(s, "BOUND_COLUMN_REF", "BOUND_COLUMN_REF", alias);
                s.write_property(200, "return_type", return_type);
                s.write_property(
                    201,
                    "binding",
                    &W(|s| column_binding(s, binding.table_index, binding.column_index), false),
                );
                s.write_property_with_default(202, "depth", &(*depth as u64));
            }
            BoundExprKind::Reference(index) => {
                Self::header(s, "BOUND_REF", "BOUND_REF", alias);
                s.write_property(200, "return_type", return_type);
                s.write_property_with_default(201, "index", &(*index as u64));
            }
            BoundExprKind::Cast { child, try_cast } => {
                let try_cast = *try_cast;
                let data = move |s: &mut BinarySerializer| {
                    s.write_property(100, "try_cast", &try_cast);
                    s.write_property(101, "is_default_cast", &false);
                };
                let arguments = [child.return_type.clone()];
                let function = FunctionInfo {
                    name: "__cast",
                    arguments: &arguments,
                    named: Vec::new(),
                    data: Some(&data),
                };
                let child = |s: &mut BinarySerializer| self.expr(s, child);
                self.function_expr(s, "OPERATOR_CAST", alias, return_type, &[&child], &function, true);
            }
            BoundExprKind::Comparison { op, left, right } => {
                let (ty, name) = comparison_names(*op);
                let data = |_: &mut BinarySerializer| {};
                let arguments = [left.return_type.clone(), right.return_type.clone()];
                let function = FunctionInfo {
                    name,
                    arguments: &arguments,
                    named: Vec::new(),
                    data: Some(&data),
                };
                let left = |s: &mut BinarySerializer| self.expr(s, left);
                let right = |s: &mut BinarySerializer| self.expr(s, right);
                self.function_expr(s, ty, alias, return_type, &[&left, &right], &function, true);
            }
            BoundExprKind::Between { input, lower, upper } => {
                let data = |s: &mut BinarySerializer| {
                    s.write_property(100, "lower_inclusive", &true);
                    s.write_property(101, "upper_inclusive", &true);
                };
                let arguments = [
                    input.return_type.clone(),
                    lower.return_type.clone(),
                    upper.return_type.clone(),
                ];
                let function = FunctionInfo {
                    name: "__between",
                    arguments: &arguments,
                    named: Vec::new(),
                    data: Some(&data),
                };
                let input = |s: &mut BinarySerializer| self.expr(s, input);
                let lower = |s: &mut BinarySerializer| self.expr(s, lower);
                let upper = |s: &mut BinarySerializer| self.expr(s, upper);
                self.function_expr(
                    s,
                    "COMPARE_BETWEEN",
                    alias,
                    return_type,
                    &[&input, &lower, &upper],
                    &function,
                    false,
                );
            }
            BoundExprKind::Conjunction { and, children } => {
                let ty = if *and { "CONJUNCTION_AND" } else { "CONJUNCTION_OR" };
                Self::header(s, "BOUND_CONJUNCTION", ty, alias);
                self.exprs_property(s, 200, "children", children);
            }
            BoundExprKind::Operator { op, children } => {
                Self::header(s, "BOUND_OPERATOR", operator_type_name(*op), alias);
                s.write_property(200, "return_type", return_type);
                self.exprs_property(s, 201, "children", children);
            }
            BoundExprKind::Case { checks, else_expr } => {
                Self::header(s, "BOUND_CASE", "CASE_EXPR", alias);
                s.write_property(200, "return_type", return_type);
                s.write_property_with_default(
                    201,
                    "case_checks",
                    &W(
                        |s| {
                            list(s, checks, |s, (when, then)| {
                                s.write_object_value(|s| {
                                    self.optional_expr_property(s, 100, "when_expr", Some(when));
                                    self.optional_expr_property(s, 101, "then_expr", Some(then));
                                })
                            })
                        },
                        checks.is_empty(),
                    ),
                );
                self.optional_expr_property(s, 202, "else_expr", Some(else_expr));
            }
            BoundExprKind::Sequence {
                op,
                sequence,
                arguments,
            } => {
                let name = match op {
                    SequenceOp::Next => "nextval",
                    SequenceOp::Current => "currval",
                    SequenceOp::Set => "setval",
                };
                let mut children = vec![BoundExpr::constant(Value::varchar(sequence.name.clone()))];
                children.extend(arguments.iter().cloned());
                let types: Vec<LogicalType> = children.iter().map(|c| c.return_type.clone()).collect();
                let function = FunctionInfo {
                    name,
                    arguments: &types,
                    named: Vec::new(),
                    data: None,
                };
                let writers: Vec<Writer<'_>> = children
                    .iter()
                    .map(|c| Box::new(move |s: &mut BinarySerializer| self.expr(s, c)) as Box<dyn Fn(&mut _)>)
                    .collect();
                let refs: Vec<&dyn Fn(&mut BinarySerializer)> = writers.iter().map(|w| &**w as _).collect();
                self.function_expr(s, "BOUND_FUNCTION", alias, return_type, &refs, &function, false);
            }
            BoundExprKind::Function {
                function,
                children,
                is_operator,
            } => {
                let info = FunctionInfo {
                    name: &function.name,
                    arguments: &function.arguments,
                    named: named_arguments(&function.name, return_type, children.len()),
                    data: None,
                };
                let writers: Vec<Writer<'_>> = children
                    .iter()
                    .map(|c| Box::new(move |s: &mut BinarySerializer| self.expr(s, c)) as Box<dyn Fn(&mut _)>)
                    .collect();
                let refs: Vec<&dyn Fn(&mut BinarySerializer)> = writers.iter().map(|w| &**w as _).collect();
                self.function_expr(s, "BOUND_FUNCTION", alias, return_type, &refs, &info, *is_operator);
            }
            BoundExprKind::ListLambda {
                kind,
                list: input,
                initial,
                captures,
                body,
            } => {
                let (name, parameters) = match kind {
                    LambdaKind::Transform => ("list_transform", 1u64),
                    LambdaKind::Filter => ("list_filter", 1),
                    LambdaKind::Reduce => ("list_reduce", 2),
                };
                // `BoundLambdaExpression::Serialize`
                let lambda = |s: &mut BinarySerializer| {
                    s.write_object_value(|s| {
                        Self::header(s, "BOUND_LAMBDA", "LAMBDA", "");
                        s.write_property(200, "return_type", &body.return_type);
                        self.optional_expr_property(s, 201, "lambda_expr", Some(body));
                        self.exprs_property(s, 202, "captures", captures);
                        s.write_property_with_default(203, "parameter_count", &parameters);
                        s.write_property_with_default(204, "parameter_names", &Vec::<String>::new());
                    })
                };
                let list_child = |s: &mut BinarySerializer| self.expr(s, input);
                let mut arguments = vec![input.return_type.clone(), LogicalType::new(LogicalTypeId::Lambda)];
                let initial_child = initial.as_ref().map(|initial| {
                    arguments.push(initial.return_type.clone());
                    move |s: &mut BinarySerializer| self.expr(s, initial)
                });
                let mut children: Vec<&dyn Fn(&mut BinarySerializer)> = vec![&list_child, &lambda];
                if let Some(initial) = &initial_child {
                    children.push(initial);
                }
                let function = FunctionInfo {
                    name,
                    arguments: &arguments,
                    named: Vec::new(),
                    data: None,
                };
                self.function_expr(s, "BOUND_FUNCTION", alias, return_type, &children, &function, false);
            }
            BoundExprKind::Subquery { .. } => {
                self.fail(Error::not_implemented(
                    "a subquery that is not planned into the query cannot be serialized",
                ));
                Self::header(s, "BOUND_SUBQUERY", "SUBQUERY", alias);
            }
        }
    }

    /// `BoundOrderByNode::Serialize`.
    fn order(&self, s: &mut BinarySerializer, modifiers: OrderModifiers, expr: &BoundExpr) {
        s.write_object_value(|s| {
            let order = match modifiers.order {
                OrderType::Ascending => "ASCENDING",
                OrderType::Descending => "DESCENDING",
            };
            let nulls = match modifiers.nulls {
                NullOrder::NullsFirst => "NULLS_FIRST",
                NullOrder::NullsLast => "NULLS_LAST",
            };
            s.write_property(100, "type", order);
            s.write_property(101, "null_order", nulls);
            self.optional_expr_property(s, 102, "expression", Some(expr));
        });
    }

    fn orders_property(&self, s: &mut BinarySerializer, field: u16, tag: &str, orders: &[(BoundExpr, OrderModifiers)]) {
        s.write_property_with_default(
            field,
            tag,
            &W(
                |s| list(s, orders, |s, (expr, modifiers)| self.order(s, *modifiers, expr)),
                orders.is_empty(),
            ),
        );
    }

    fn order_nodes_property(&self, s: &mut BinarySerializer, field: u16, tag: &str, orders: &[BoundOrderByNode]) {
        let orders: Vec<(BoundExpr, OrderModifiers)> = orders.iter().map(|o| (o.expr.clone(), o.modifiers)).collect();
        self.orders_property(s, field, tag, &orders);
    }

    /// `BoundOrderModifier` behind a `unique_ptr`: null without orders.
    fn order_modifier_property(
        &self,
        s: &mut BinarySerializer,
        field: u16,
        tag: &str,
        orders: &[(BoundExpr, OrderModifiers)],
    ) {
        s.write_property_with_default(
            field,
            tag,
            &W(
                |s| {
                    if orders.is_empty() {
                        s.begin_nullable(false);
                        return;
                    }
                    s.begin_nullable(true);
                    s.write_object_value(|s| self.orders_property(s, 100, "orders", orders));
                },
                orders.is_empty(),
            ),
        );
    }

    /// `BoundAggregateExpression::Serialize`.
    fn aggregate(&self, s: &mut BinarySerializer, aggregate: &BoundAggregate) {
        s.write_object_value(|s| {
            Self::header(s, "BOUND_AGGREGATE", "BOUND_AGGREGATE", "");
            s.write_property(200, "return_type", &aggregate.return_type);
            s.write_property(
                201,
                "children",
                &W(
                    |s| list(s, &aggregate.children, |s, e| self.expr_ptr(s, e)),
                    aggregate.children.is_empty(),
                ),
            );
            let function = FunctionInfo {
                name: &aggregate.function.name,
                arguments: &aggregate.function.arguments,
                named: Vec::new(),
                data: None,
            };
            Self::function(s, &function, aggregate.children.len());
            s.write_property(
                203,
                "aggregate_type",
                if aggregate.distinct { "DISTINCT" } else { "NON_DISTINCT" },
            );
            self.optional_expr_property(s, 204, "filter", aggregate.filter.as_ref());
            self.order_modifier_property(s, 205, "order_bys", &aggregate.order_by);
            let export = if aggregate.function.export.is_some() {
                "STATE_EXPORT"
            } else {
                "NONE"
            };
            s.write_property_with_explicit_default(206, "state_export", &export.to_string(), &"NONE".to_string());
        });
    }

    /// `BoundWindowExpression::Serialize`.
    fn window(&self, s: &mut BinarySerializer, window: &BoundWindow) {
        s.write_object_value(|s| {
            Self::header(s, "BOUND_WINDOW", window_type_name(window.function), "");
            s.write_property(200, "return_type", &window.return_type);
            s.write_property(
                201,
                "children",
                &W(
                    |s| list(s, &window.children, |s, e| self.expr_ptr(s, e)),
                    window.children.is_empty(),
                ),
            );
            if let Some(aggregate) = &window.aggregate {
                let function = FunctionInfo {
                    name: &aggregate.name,
                    arguments: &aggregate.arguments,
                    named: Vec::new(),
                    data: None,
                };
                Self::function(s, &function, window.children.len());
            }
            s.write_property(
                202,
                "partitions",
                &W(
                    |s| list(s, &window.partitions, |s, e| self.expr_ptr(s, e)),
                    window.partitions.is_empty(),
                ),
            );
            s.write_property(
                203,
                "orders",
                &W(
                    |s| list(s, &window.orders, |s, (e, m)| self.order(s, *m, e)),
                    window.orders.is_empty(),
                ),
            );
            self.optional_expr_property(s, 204, "filters", window.filter.as_ref());
            s.write_property(205, "ignore_nulls", &window.ignore_nulls);
            s.write_property(206, "start", window_boundary_name(window.start));
            s.write_property(207, "end", window_boundary_name(window.end));
            self.optional_expr_property(s, 208, "start_expr", window.start_expr.as_ref());
            self.optional_expr_property(s, 209, "end_expr", window.end_expr.as_ref());
            self.optional_expr_property(s, 210, "offset_expr", None);
            self.optional_expr_property(s, 211, "default_expr", None);
            let exclude = match window.exclude {
                ast::WindowExclude::NoOther => "NO_OTHER",
                ast::WindowExclude::CurrentRow => "CURRENT_ROW",
                ast::WindowExclude::Group => "GROUP",
                ast::WindowExclude::Ties => "TIES",
            };
            s.write_property(212, "exclude_clause", exclude);
            s.write_property(213, "distinct", &window.distinct);
            s.write_property(
                214,
                "arg_orders",
                &W(
                    |s| list(s, &window.arg_orders, |s, (e, m)| self.order(s, *m, e)),
                    window.arg_orders.is_empty(),
                ),
            );
        });
    }

    // ---------------------------------------------------------------------------------------------
    // operators
    // ---------------------------------------------------------------------------------------------

    fn op(&self, s: &mut BinarySerializer, op: &LogicalOperator) {
        // `ColumnSelect` is a projection map of the operator above in DuckDB
        if let K::ColumnSelect { .. } = &op.kind
            && let [child] = op.children.as_slice()
        {
            return self.op(s, child);
        }
        s.write_object_value(|s| self.op_body(s, op));
    }

    fn type_name(&self, op: &LogicalOperator) -> &'static str {
        match &op.kind {
            K::Get { .. }
            | K::Range { .. }
            | K::CsvScan { .. }
            | K::JsonScan { .. }
            | K::ParquetScan { .. }
            | K::UserTableScan { .. } => "LOGICAL_GET",
            K::CopyToParquet { .. } | K::CopyToFile { .. } => "LOGICAL_COPY_TO_FILE",
            K::EmptyResult { .. } => "LOGICAL_EMPTY_RESULT",
            K::DummyScan { .. } => "LOGICAL_DUMMY_SCAN",
            K::ChunkGet { .. } => "LOGICAL_CHUNK_GET",
            K::ExpressionGet { materialized, .. } => {
                if *materialized {
                    "LOGICAL_CHUNK_GET"
                } else {
                    "LOGICAL_EXPRESSION_GET"
                }
            }
            K::Filter { .. } => "LOGICAL_FILTER",
            K::Window { .. } => "LOGICAL_WINDOW",
            K::MatchRecognize { .. } => "LOGICAL_MATCH_RECOGNIZE",
            K::Aggregate { .. } => "LOGICAL_AGGREGATE_AND_GROUP_BY",
            K::Order { .. } => "LOGICAL_ORDER_BY",
            K::TopN { .. } => "LOGICAL_TOP_N",
            K::Limit { .. } | K::LimitPercent { .. } => "LOGICAL_LIMIT",
            K::Sample { .. } => "LOGICAL_SAMPLE",
            K::Distinct { .. } => "LOGICAL_DISTINCT",
            K::SecureView { .. } => "LOGICAL_SECURE_VIEW",
            K::Unnest { .. } => "LOGICAL_UNNEST",
            K::Union { .. } => "LOGICAL_UNION",
            K::ExceptIntersect { except: true, .. } => "LOGICAL_EXCEPT",
            K::ExceptIntersect { except: false, .. } => "LOGICAL_INTERSECT",
            K::RecursiveCte { .. } => "LOGICAL_RECURSIVE_CTE",
            K::WorkingTable { .. } | K::CteRef { .. } => "LOGICAL_CTE_REF",
            K::MaterializedCte { .. } => "LOGICAL_MATERIALIZED_CTE",
            K::DelimGet { .. } => "LOGICAL_DELIM_GET",
            K::DelimJoin { .. } => "LOGICAL_DELIM_JOIN",
            K::LateralJoin { .. } => "LOGICAL_DEPENDENT_JOIN",
            K::ColumnSelect { .. } | K::Projection { .. } => "LOGICAL_PROJECTION",
            K::CrossProduct => "LOGICAL_CROSS_PRODUCT",
            K::Join { conditions, .. } | K::MarkJoin { conditions, .. } => {
                if conditions.is_empty() {
                    "LOGICAL_ANY_JOIN"
                } else {
                    "LOGICAL_COMPARISON_JOIN"
                }
            }
            K::AsOfJoin { .. } => "LOGICAL_ASOF_JOIN",
            K::PositionalJoin => "LOGICAL_POSITIONAL_JOIN",
            K::Insert { .. } => "LOGICAL_INSERT",
            K::Delete { .. } => "LOGICAL_DELETE",
            K::Update { .. } => "LOGICAL_UPDATE",
            K::MergeInto { .. } => "LOGICAL_MERGE_INTO",
            K::CreateTableAs { .. } | K::CreateTable { .. } => "LOGICAL_CREATE_TABLE",
        }
    }

    fn op_body(&self, s: &mut BinarySerializer, op: &LogicalOperator) {
        let name = self.type_name(op);
        let mut children: Vec<&LogicalOperator> = op.children.iter().collect();
        if let K::RecursiveCte { recursive, .. } = &op.kind {
            children.push(recursive);
        }
        s.write_property(100, "type", name);
        s.write_property_with_default(
            101,
            "children",
            &W(
                |s| {
                    list(s, &children, |s, child| {
                        s.begin_nullable(true);
                        self.op(s, child);
                    })
                },
                children.is_empty(),
            ),
        );
        let table_index = |s: &mut BinarySerializer, field: u16, index: usize| {
            s.write_property_with_default(field, "table_index", &(index as u64));
        };
        match &op.kind {
            K::Get {
                table,
                table_index,
                column_ids,
                filters,
                zone_filters,
                optional_filters,
                projection,
                index_rows,
                sample,
                ..
            } => {
                let types: Vec<LogicalType> = table.columns.iter().map(|c| c.ty.clone()).collect();
                let names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
                let data = |s: &mut BinarySerializer| {
                    // `TableScanSerialize`
                    s.write_property(100, "catalog", table.catalog.as_str());
                    s.write_property(101, "schema", table.schema.as_str());
                    s.write_property(102, "table", table.name.as_str());
                    s.write_property(103, "is_index_scan", &index_rows.is_some());
                    s.write_property(104, "is_create_index", &false);
                    let rows: Vec<i64> = index_rows
                        .as_ref()
                        .map(|rows| rows.iter().map(|r| *r as i64).collect())
                        .unwrap_or_default();
                    s.write_property_with_default(105, "result_ids", &rows);
                    let path = [table.catalog.clone(), table.schema.clone(), table.name.clone()];
                    s.write_property_with_default(106, "qualified_name", &W(|s| qualified_name(s, &path), false));
                };
                let mut all_filters: Vec<&BoundExpr> = filters.iter().collect();
                all_filters.extend(optional_filters);
                let get = GetFields {
                    table_index: *table_index,
                    types,
                    names,
                    column_ids: column_ids.clone(),
                    projection: projection.clone().unwrap_or_default(),
                    filters: all_filters,
                    zone_filters: zone_filters.iter().collect(),
                    function: "seq_scan",
                    parameters: Vec::new(),
                    data: Some(&data),
                    sample: sample.as_ref().map(|sample| (&sample.options, sample.rate)),
                };
                self.get(s, &get);
            }
            K::Range {
                table_index,
                generate_series,
                start,
                increment,
                count,
            } => {
                let ty = op.types().into_iter().next().unwrap_or(LogicalType::BIGINT);
                let value = |v: i128| match i64::try_from(v) {
                    Ok(v) if ty.id() == LogicalTypeId::BigInt => Value::bigint(v),
                    _ => Value::hugeint(v),
                };
                let count = *count as i128;
                let end = if *generate_series {
                    start + (count - 1) * increment
                } else {
                    start + count * increment
                };
                let parameters = if *start == 0 && *increment == 1 {
                    vec![value(end)]
                } else {
                    vec![value(*start), value(end), value(*increment)]
                };
                let name = if *generate_series { "generate_series" } else { "range" };
                let get = GetFields {
                    table_index: *table_index,
                    types: vec![ty],
                    names: vec![name.to_string()],
                    column_ids: vec![0],
                    projection: Vec::new(),
                    filters: Vec::new(),
                    zone_filters: Vec::new(),
                    function: name,
                    parameters,
                    data: None,
                    sample: None,
                };
                self.get(s, &get);
            }
            K::CsvScan { table_index, scan } => {
                let files: Vec<&str> = scan.files.iter().map(|f| f.path.as_str()).collect();
                self.file_get(
                    s,
                    *table_index,
                    "read_csv",
                    &files,
                    &scan.names,
                    &scan.types,
                    scan.projection.as_deref(),
                );
            }
            K::JsonScan { table_index, scan } => {
                let files: Vec<&str> = scan.files.iter().map(String::as_str).collect();
                self.file_get(
                    s,
                    *table_index,
                    &scan.function_name,
                    &files,
                    &scan.names,
                    &scan.types,
                    scan.projection.as_deref(),
                );
            }
            K::ParquetScan { table_index, scan } => {
                let files: Vec<&str> = scan.files.iter().map(String::as_str).collect();
                self.file_get(
                    s,
                    *table_index,
                    &scan.function_name,
                    &files,
                    &scan.names,
                    &scan.types,
                    None,
                );
            }
            K::CopyToFile {
                path, names, options, ..
            } => {
                let format = if options.json_names.is_some() { "json" } else { "csv" };
                self.copy_to_file(s, op, path, names, format);
            }
            K::CopyToParquet { path, names, .. } => self.copy_to_file(s, op, path, names, "parquet"),
            K::EmptyResult { types, bindings } => {
                s.write_property_with_default(200, "return_types", types);
                s.write_property_with_default(
                    201,
                    "bindings",
                    &W(
                        |s| list(s, bindings, |s, b| column_binding(s, b.table_index, b.column_index)),
                        bindings.is_empty(),
                    ),
                );
            }
            K::DummyScan { table_index: index } => table_index(s, 200, *index),
            // `LogicalGet` of the function, whose bind data a plan cannot keep (`SupportStatementCache` is false)
            K::UserTableScan {
                table_index,
                types,
                scan,
            } => {
                let get = GetFields {
                    table_index: *table_index,
                    types: types.clone(),
                    names: op.column_bindings().iter().map(|b| b.column_index.to_string()).collect(),
                    column_ids: (0..types.len()).collect(),
                    projection: Vec::new(),
                    filters: Vec::new(),
                    zone_filters: Vec::new(),
                    function: &scan.name,
                    parameters: Vec::new(),
                    data: None,
                    sample: None,
                };
                self.get(s, &get);
            }
            K::ChunkGet {
                table_index: index,
                types,
                chunks,
            } => {
                let mut values: Vec<Vec<Value>> = vec![Vec::new(); types.len()];
                for chunk in chunks.iter() {
                    for (column, values) in values.iter_mut().enumerate().take(chunk.column_count()) {
                        values.extend((0..chunk.len()).map(|row| chunk.get_value(column, row)));
                    }
                }
                self.chunk_get(s, *index, types, &values);
            }
            K::ExpressionGet {
                table_index: index,
                types,
                expressions,
                materialized,
                ..
            } => {
                let constants: Option<Vec<Vec<Value>>> = materialized
                    .then(|| {
                        expressions
                            .iter()
                            .map(|row| {
                                row.iter()
                                    .map(|e| match &e.kind {
                                        BoundExprKind::Constant(v) => Some(v.clone()),
                                        _ => None,
                                    })
                                    .collect::<Option<Vec<Value>>>()
                            })
                            .collect()
                    })
                    .flatten();
                match constants {
                    Some(rows) => {
                        let mut values: Vec<Vec<Value>> = vec![Vec::new(); types.len()];
                        for row in rows {
                            for (column, value) in row.into_iter().enumerate().take(types.len()) {
                                values[column].push(value);
                            }
                        }
                        self.chunk_get(s, *index, types, &values);
                    }
                    None => {
                        table_index(s, 200, *index);
                        s.write_property_with_default(201, "expr_types", types);
                        s.write_property_with_default(
                            202,
                            "expressions",
                            &W(
                                |s| {
                                    list(s, expressions, |s, row| {
                                        list(s, row, |s, e| self.expr_ptr(s, e));
                                    })
                                },
                                expressions.is_empty(),
                            ),
                        );
                    }
                }
            }
            K::Filter { expressions } => {
                self.exprs_property(s, 200, "expressions", expressions);
                s.write_property_with_default(201, "projection_map", &Vec::<u64>::new());
            }
            K::Window {
                table_index: index,
                windows,
            } => {
                s.write_property_with_default(200, "window_index", &(*index as u64));
                s.write_property_with_default(
                    201,
                    "expressions",
                    &W(
                        |s| {
                            list(s, windows, |s, w| {
                                s.begin_nullable(true);
                                self.window(s, w);
                            })
                        },
                        windows.is_empty(),
                    ),
                );
            }
            K::Aggregate {
                group_index,
                aggregate_index,
                groupings_index,
                groups,
                aggregates,
                grouping_sets,
                grouping_functions,
                ..
            } => {
                s.write_property_with_default(
                    200,
                    "expressions",
                    &W(
                        |s| {
                            list(s, aggregates, |s, a| {
                                s.begin_nullable(true);
                                self.aggregate(s, a);
                            })
                        },
                        aggregates.is_empty(),
                    ),
                );
                s.write_property_with_default(201, "group_index", &(*group_index as u64));
                s.write_property_with_default(202, "aggregate_index", &(*aggregate_index as u64));
                s.write_property_with_default(203, "groupings_index", &(*groupings_index as u64));
                self.exprs_property(s, 204, "groups", groups);
                let sets: Vec<Vec<u64>> = grouping_sets.iter().map(|set| u64s(set.iter().copied())).collect();
                s.write_property_with_default(205, "grouping_sets", &sets);
                let functions: Vec<Vec<u64>> = grouping_functions.iter().map(|f| u64s(f.iter().copied())).collect();
                s.write_property_with_default(206, "grouping_functions", &functions);
                let validity = "CAN_HAVE_NULL_VALUES".to_string();
                s.write_property_with_explicit_default(207, "distinct_validity", &validity, &validity);
            }
            K::Order { orders } => {
                self.order_nodes_property(s, 200, "orders", orders);
                s.write_property_with_default(201, "projections", &Vec::<u64>::new());
            }
            K::TopN {
                orders, limit, offset, ..
            } => {
                self.order_nodes_property(s, 200, "orders", orders);
                s.write_property_with_default(201, "limit", limit);
                s.write_property_with_default(202, "offset", offset);
                s.write_property_with_explicit_default(203, "unpruned_offset", &INVALID_INDEX, &INVALID_INDEX);
            }
            K::Limit { limit, offset, failure } => {
                let limit_node = match (failure, limit) {
                    (Some(_), _) => LimitNode::Expression,
                    (None, Some(limit)) => LimitNode::Value(*limit),
                    (None, None) => LimitNode::Unset,
                };
                Self::limit_node(s, 200, "limit_val", limit_node);
                let offset_node = if *offset == 0 {
                    LimitNode::Unset
                } else {
                    LimitNode::Value(*offset)
                };
                Self::limit_node(s, 201, "offset_val", offset_node);
                s.write_property_with_explicit_default(202, "unpruned_offset", &INVALID_INDEX, &INVALID_INDEX);
            }
            K::LimitPercent { percent, offset } => {
                Self::limit_node(s, 200, "limit_val", LimitNode::Percentage(*percent));
                let offset_node = if *offset == 0 {
                    LimitNode::Unset
                } else {
                    LimitNode::Value(*offset)
                };
                Self::limit_node(s, 201, "offset_val", offset_node);
                s.write_property_with_explicit_default(202, "unpruned_offset", &INVALID_INDEX, &INVALID_INDEX);
            }
            K::Sample { options } => {
                s.write_property_with_default(
                    200,
                    "sample_options",
                    &W(
                        |s| {
                            s.begin_nullable(true);
                            sample_options(s, options, -1.0);
                        },
                        false,
                    ),
                );
            }
            K::Distinct { targets, on, orders } => {
                s.write_property(200, "distinct_type", if *on { "DISTINCT_ON" } else { "DISTINCT" });
                self.exprs_property(s, 201, "distinct_targets", targets);
                let orders: Vec<(BoundExpr, OrderModifiers)> =
                    orders.iter().map(|o| (o.expr.clone(), o.modifiers)).collect();
                self.order_modifier_property(s, 202, "order_by", &orders);
            }
            K::SecureView {
                view_name,
                pushed_filters,
                ..
            } => {
                s.write_property_with_default(200, "view_name", view_name.as_str());
                s.write_property_with_default(201, "pushed_filters", pushed_filters);
                s.write_property_with_default(202, "has_source", &false);
                s.write_property_with_default(204, "source_types", &Vec::<LogicalType>::new());
                s.write_property_with_default(205, "has_at_clause", &false);
                s.write_property_with_default(206, "at_unit", "");
            }
            K::Unnest {
                table_index: index,
                expressions,
            } => {
                s.write_property_with_default(200, "unnest_index", &(*index as u64));
                // `BoundUnnestExpression`: an element of each list
                let unnest = |s: &mut BinarySerializer, list: &BoundExpr| {
                    s.begin_nullable(true);
                    s.write_object_value(|s| {
                        Self::header(s, "BOUND_UNNEST", "BOUND_UNNEST", &list.alias);
                        let element = list
                            .return_type
                            .child_type()
                            .cloned()
                            .unwrap_or_else(|| list.return_type.clone());
                        s.write_property(200, "return_type", &element);
                        self.optional_expr_property(s, 201, "child", Some(list));
                    });
                };
                s.write_property_with_default(
                    201,
                    "expressions",
                    &W(|s| list(s, expressions, unnest), expressions.is_empty()),
                );
            }
            K::Union {
                table_index: index,
                types,
            } => Self::set_operation(s, *index, types.len(), true),
            K::ExceptIntersect {
                table_index: index,
                types,
                all,
                ..
            } => Self::set_operation(s, *index, types.len(), *all),
            K::RecursiveCte {
                name,
                table_index: index,
                types,
                union_all,
                key,
                ..
            } => {
                s.write_property_with_default(200, "union_all", union_all);
                s.write_property_with_default(201, "ctename", name.as_str());
                table_index(s, 202, *index);
                s.write_property_with_default(203, "column_count", &(types.len() as u64));
                let keys: Vec<BoundExpr> = key
                    .keys
                    .iter()
                    .map(|k| {
                        let ty = key.internal_types.get(*k).or(types.get(*k)).cloned();
                        BoundExpr::new(BoundExprKind::Reference(*k), ty.unwrap_or(LogicalType::SQLNULL))
                    })
                    .collect();
                self.exprs_property(s, 204, "key_targets", &keys);
                s.write_property_with_default(205, "ref_recurring", &false);
                s.write_property_with_default(
                    206,
                    "payload_aggregates",
                    &W(
                        |s| {
                            list(s, &key.payload, |s, (_, aggregate)| {
                                s.begin_nullable(true);
                                self.aggregate(s, aggregate);
                            })
                        },
                        key.payload.is_empty(),
                    ),
                );
                s.write_property_with_default(207, "internal_types", &key.internal_types);
            }
            K::WorkingTable {
                table_index: index,
                types,
                working,
            } => {
                let cte = self
                    .ctes
                    .get(&(std::sync::Arc::as_ptr(working) as usize))
                    .copied()
                    .unwrap_or_default();
                Self::cte_ref(s, *index, cte, types, &[]);
            }
            K::CteRef {
                table_index: index,
                cte_index,
                types,
                names,
                ..
            } => Self::cte_ref(s, *index, *cte_index, types, names),
            K::MaterializedCte {
                name,
                table_index: index,
                always,
                ..
            } => {
                table_index(s, 200, *index);
                let columns = op.children.first().map_or(0, |c| c.types().len());
                s.write_property_with_default(201, "column_count", &(columns as u64));
                s.write_property_with_default(202, "ctename", name.as_str());
                let materialize = if *always {
                    "CTE_MATERIALIZE_ALWAYS"
                } else {
                    "CTE_MATERIALIZE_DEFAULT"
                };
                s.write_property_with_explicit_default(
                    203,
                    "materialize",
                    &materialize.to_string(),
                    &"CTE_MATERIALIZE_DEFAULT".to_string(),
                );
            }
            K::DelimGet {
                table_index: index,
                types,
                ..
            } => {
                table_index(s, 200, *index);
                s.write_property_with_default(201, "chunk_types", types);
            }
            K::DelimJoin {
                join_type,
                conditions,
                predicate,
                mark_index,
                ..
            } => {
                let duplicates: Vec<BoundExpr> = conditions.iter().map(|c| c.left.clone()).collect();
                self.comparison_join(
                    s,
                    *join_type,
                    mark_index.unwrap_or(0),
                    conditions,
                    None,
                    predicate.as_ref(),
                    &duplicates,
                );
            }
            K::Join {
                join_type,
                conditions,
                predicate,
                ..
            } => self.join(s, *join_type, 0, conditions, predicate.as_ref()),
            K::MarkJoin {
                table_index: index,
                conditions,
                predicate,
            } => self.join(s, JoinType::Mark, *index, conditions, predicate.as_ref()),
            K::AsOfJoin {
                join_type,
                conditions,
                left,
                op: comparison,
                right,
                predicate,
            } => {
                let inequality = (left, right, *comparison);
                self.comparison_join(s, *join_type, 0, conditions, Some(inequality), predicate.as_ref(), &[]);
            }
            K::Projection {
                table_index: index,
                expressions,
            } => {
                table_index(s, 200, *index);
                self.exprs_property(s, 201, "expressions", expressions);
            }
            K::CrossProduct | K::PositionalJoin | K::ColumnSelect { .. } => {}
            K::Insert {
                table,
                column_index_map,
                table_index: index,
                returning,
                on_conflict,
            } => {
                self.table_info(s, table);
                s.write_property_with_default(201, "insert_values", &Vec::<u64>::new());
                let map: Vec<u64> = column_index_map
                    .iter()
                    .map(|c| c.map_or(INVALID_INDEX, |c| c as u64))
                    .collect();
                s.write_property_with_default(202, "column_index_map", &map);
                let types: Vec<LogicalType> = table.columns.iter().map(|c| c.ty.clone()).collect();
                s.write_property_with_default(203, "expected_types", &types);
                table_index(s, 204, *index);
                s.write_property_with_default(205, "return_chunk", returning);
                s.write_property_with_default(206, "bound_defaults", &Vec::<u64>::new());
                let action = match on_conflict {
                    None => "THROW",
                    Some(conflict) if conflict.do_update => "UPDATE",
                    Some(_) => "NOTHING",
                };
                s.write_property(207, "action_type", action);
                s.write_property_with_default(208, "expected_set_types", &Vec::<LogicalType>::new());
                s.write_property_with_default(209, "on_conflict_filter", &Vec::<u64>::new());
                self.optional_expr_property(
                    s,
                    210,
                    "on_conflict_condition",
                    on_conflict.as_ref().and_then(|c| c.condition.as_ref()),
                );
                self.optional_expr_property(s, 211, "do_update_condition", None);
                let set_columns = on_conflict.as_ref().map(|c| u64s(c.set_columns.iter().copied()));
                s.write_property_with_default(212, "set_columns", &set_columns.unwrap_or_default());
                s.write_property_with_default(213, "set_types", &Vec::<LogicalType>::new());
                s.write_property_with_default(214, "excluded_table_index", &0u64);
                s.write_property_with_default(215, "columns_to_fetch", &Vec::<u64>::new());
                s.write_property_with_default(216, "source_columns", &Vec::<u64>::new());
                let set_expressions = on_conflict.as_ref().map(|c| c.set_expressions.as_slice());
                self.exprs_property(s, 217, "expressions", set_expressions.unwrap_or_default());
                s.write_property_with_explicit_default(218, "update_is_del_and_insert", &false, &false);
            }
            K::Delete {
                table,
                table_index: index,
                returning,
            } => {
                self.table_info(s, table);
                table_index(s, 201, *index);
                s.write_property_with_default(202, "return_chunk", returning);
                s.write_property_with_default(203, "expressions", &Vec::<u64>::new());
                s.write_property_with_default(204, "return_columns", &Vec::<u64>::new());
            }
            K::Update {
                table,
                table_index: index,
                columns,
                returning,
            } => {
                self.table_info(s, table);
                table_index(s, 201, *index);
                s.write_property_with_default(202, "return_chunk", returning);
                // the new values are the child's columns, then the row id
                let types = op.children.first().map(|c| c.types()).unwrap_or_default();
                let values: Vec<BoundExpr> = (0..columns.len())
                    .map(|i| {
                        let ty = types.get(i).cloned().unwrap_or(LogicalType::SQLNULL);
                        BoundExpr::new(BoundExprKind::Reference(i), ty)
                    })
                    .collect();
                self.exprs_property(s, 203, "expressions", &values);
                s.write_property_with_default(204, "columns", &u64s(columns.iter().copied()));
                s.write_property_with_default(205, "bound_defaults", &Vec::<u64>::new());
                s.write_property_with_default(206, "update_is_del_and_insert", &false);
                s.write_property_with_explicit_default(207, "capture_old_rows", &false, &false);
                s.write_property_with_default(208, "old_row_columns", &Vec::<u64>::new());
                let handling = "ASSUME_UNIQUE".to_string();
                s.write_property_with_explicit_default(209, "row_id_handling", &handling, &handling);
            }
            K::MergeInto {
                table,
                table_index: index,
                actions,
                returning,
                ..
            } => {
                self.table_info(s, table);
                table_index(s, 201, *index);
                s.write_property_with_default(202, "bound_defaults", &Vec::<u64>::new());
                s.write_property_with_default(203, "row_id_start", &0u64);
                s.write_property(204, "source_marker", &INVALID_INDEX);
                let entry = |s: &mut BinarySerializer, (condition, actions): &(MergeCondition, Vec<_>)| {
                    s.write_object_value(|s| {
                        let condition = match condition {
                            MergeCondition::WhenMatched => "WHEN_MATCHED",
                            MergeCondition::WhenNotMatchedBySource => "WHEN_NOT_MATCHED_BY_SOURCE",
                            MergeCondition::WhenNotMatchedByTarget => "WHEN_NOT_MATCHED_BY_TARGET",
                        };
                        s.write_property(0, "key", condition);
                        s.write_property(
                            1,
                            "value",
                            &W(|s| list(s, actions, |s, a| self.merge_action(s, a)), false),
                        );
                    })
                };
                s.write_property_with_default(205, "actions", &W(|s| list(s, actions, entry), actions.is_empty()));
                s.write_property_with_default(206, "return_chunk", returning);
                s.write_property_with_default(207, "delete_return_columns", &Vec::<u64>::new());
            }
            K::CreateTableAs {
                schema, name, columns, ..
            }
            | K::CreateTable {
                schema, name, columns, ..
            } => {
                let path = [schema.name.clone(), name.clone()];
                match create_table_info(&path, name, columns) {
                    Ok(info) => s.write_property_with_default(200, "info", &W(|s| ptr(s, &info), false)),
                    Err(error) => self.fail(error),
                }
            }
            K::LateralJoin { .. } | K::MatchRecognize { .. } => self.fail(Error::new(
                ErrorKind::InvalidInput,
                format!("Operator '{name}' does not support serialization"),
            )),
        }
    }

    /// `LogicalGet::Serialize`.
    fn get(&self, s: &mut BinarySerializer, get: &GetFields<'_>) {
        s.write_property(200, "table_index", &(get.table_index as u64));
        s.write_property(201, "returned_types", &get.types);
        s.write_property(202, "names", &get.names);
        s.write_property(204, "projection_ids", &u64s(get.projection.iter().copied()));
        s.write_property(
            205,
            "table_filters",
            &W(|s| s.write_object_value(|s| self.table_filters(s, get)), false),
        );
        let arguments: Vec<LogicalType> = get.parameters.iter().map(|p| p.logical_type().clone()).collect();
        let function = FunctionInfo {
            name: get.function,
            arguments: &arguments,
            named: Vec::new(),
            data: get.data,
        };
        Self::function(s, &function, arguments.len());
        // `has_serialize` functions only write these from v2.0.0 on, which is always here
        s.write_property(206, "parameters", &get.parameters);
        s.write_property(207, "named_parameters", &Vec::<u64>::new());
        s.write_property(208, "input_table_types", &Vec::<LogicalType>::new());
        s.write_property(209, "input_table_names", &Vec::<String>::new());
        s.write_property(210, "projected_input", &Vec::<u64>::new());
        s.write_property_with_default(
            211,
            "column_indexes",
            &W(
                |s| list(s, &get.column_ids, |s, id| column_index(s, *id)),
                get.column_ids.is_empty(),
            ),
        );
        // `ExtraOperatorInfo`
        let sample = get.sample;
        s.write_property_with_default(
            212,
            "extra_info",
            &W(
                |s| {
                    s.write_object_value(|s| {
                        s.write_property_with_default(100, "file_filters", "");
                        s.write_property(101, "total_files", &INVALID_INDEX);
                        s.write_property(102, "filtered_files", &INVALID_INDEX);
                        s.write_property_with_default(
                            103,
                            "sample_options",
                            &W(
                                |s| match sample {
                                    Some((options, rate)) => {
                                        s.begin_nullable(true);
                                        sample_options(s, options, rate);
                                    }
                                    None => s.begin_nullable(false),
                                },
                                sample.is_none(),
                            ),
                        );
                    })
                },
                sample.is_none(),
            ),
        );
        s.write_property_with_explicit_default(213, "ordinality_idx", &INVALID_INDEX, &INVALID_INDEX);
        s.write_property_with_default(214, "row_group_order_options", &W(|s| s.begin_nullable(false), true));
        s.write_property_with_default(215, "scan_partition_indices", &Vec::<u64>::new());
        let ordinality = "WITHOUT_ORDINALITY".to_string();
        s.write_property_with_explicit_default(216, "source_ordinality", &ordinality, &ordinality);
        s.write_property(217, "table_filters_by_projection", &true);
        s.write_property_with_explicit_default(218, "has_pushed_projection", &false, &false);
        s.write_property_with_default(219, "at_clause", &W(|s| s.begin_nullable(false), true));
    }

    /// `TableFilterSet::Serialize`: each filter as an `EXPRESSION_FILTER` of the column it reads, by its place in
    /// `column_ids`; filters on the same column are one conjunction.
    fn table_filters(&self, s: &mut BinarySerializer, get: &GetFields<'_>) {
        let mut by_column: Vec<(usize, Vec<BoundExpr>)> = Vec::new();
        for filter in &get.filters {
            let column = first_column(filter).unwrap_or(0);
            match by_column.iter_mut().find(|(c, _)| *c == column) {
                Some((_, filters)) => filters.push((*filter).clone()),
                None => by_column.push((column, vec![(*filter).clone()])),
            }
        }
        by_column.sort_by_key(|(column, _)| *column);
        let entries: Vec<(usize, BoundExpr)> = by_column
            .into_iter()
            .map(|(column, mut filters)| {
                let filter = if filters.len() == 1 {
                    filters.pop().expect("one")
                } else {
                    BoundExpr::new(
                        BoundExprKind::Conjunction {
                            and: true,
                            children: filters,
                        },
                        LogicalType::BOOLEAN,
                    )
                };
                (column, filter)
            })
            .collect();
        let expression_filter = |s: &mut BinarySerializer, expr: &BoundExpr| {
            s.begin_nullable(true);
            s.write_object_value(|s| {
                s.write_property(100, "filter_type", "EXPRESSION_FILTER");
                self.optional_expr_property(s, 200, "expr", Some(expr));
            });
        };
        s.write_property_with_default(
            100,
            "filters",
            &W(
                |s| {
                    list(s, &entries, |s, (column, filter)| {
                        s.write_object_value(|s| {
                            s.write_property(0, "key", &(*column as u64));
                            s.write_property(1, "value", &W(|s| expression_filter(s, filter), false));
                        })
                    })
                },
                entries.is_empty(),
            ),
        );
        if !get.zone_filters.is_empty() {
            s.write_property(
                101,
                "multi_column_filters",
                &W(|s| list(s, &get.zone_filters, |s, f| expression_filter(s, f)), false),
            );
        }
    }

    /// A table function that reads files, which writes neither its bind data nor its options.
    #[allow(clippy::too_many_arguments, reason = "what a file scan has")]
    fn file_get(
        &self,
        s: &mut BinarySerializer,
        table_index: usize,
        function: &str,
        files: &[&str],
        names: &[String],
        types: &[LogicalType],
        projection: Option<&[usize]>,
    ) {
        let parameters = match files {
            [file] => vec![Value::varchar(*file)],
            files => Value::list(LogicalType::VARCHAR, files.iter().map(|f| Value::varchar(*f)).collect())
                .map(|list| vec![list])
                .unwrap_or_default(),
        };
        let column_ids = projection.map_or_else(|| (0..names.len()).collect(), <[usize]>::to_vec);
        let get = GetFields {
            table_index,
            types: types.to_vec(),
            names: names.to_vec(),
            column_ids,
            projection: Vec::new(),
            filters: Vec::new(),
            zone_filters: Vec::new(),
            function,
            parameters,
            data: None,
            sample: None,
        };
        self.get(s, &get);
    }

    /// `LogicalColumnDataGet::Serialize`.
    fn chunk_get(&self, s: &mut BinarySerializer, table_index: usize, types: &[LogicalType], values: &[Vec<Value>]) {
        s.write_property_with_default(200, "table_index", &(table_index as u64));
        s.write_property_with_default(201, "chunk_types", &types.to_vec());
        s.write_property_with_default(
            202,
            "collection",
            &W(
                |s| {
                    s.begin_nullable(true);
                    s.write_object_value(|s| {
                        s.write_property(100, "types", &types.to_vec());
                        s.write_property(101, "values", &values.to_vec());
                    });
                },
                false,
            ),
        );
        let default: Vec<u64> = u64s(0..types.len());
        let columns = self
            .chunk_columns
            .get(&table_index)
            .map_or_else(|| default.clone(), |columns| u64s(columns.iter().copied()));
        s.write_property_with_explicit_default(203, "column_ids", &columns, &default);
    }

    /// `LogicalSetOperation::Serialize`.
    fn set_operation(s: &mut BinarySerializer, table_index: usize, columns: usize, all: bool) {
        s.write_property_with_default(200, "table_index", &(table_index as u64));
        s.write_property_with_default(201, "column_count", &(columns as u64));
        s.write_property_with_explicit_default(202, "setop_all", &all, &true);
        s.write_property_with_explicit_default(203, "allow_out_of_order", &true, &true);
    }

    /// `LogicalCTERef::Serialize`.
    fn cte_ref(
        s: &mut BinarySerializer,
        table_index: usize,
        cte_index: usize,
        types: &[LogicalType],
        names: &[String],
    ) {
        s.write_property_with_default(200, "table_index", &(table_index as u64));
        s.write_property_with_default(201, "cte_index", &(cte_index as u64));
        s.write_property_with_default(202, "chunk_types", &types.to_vec());
        s.write_property_with_default(203, "bound_columns", &names.to_vec());
        s.write_property_with_default(205, "is_recurring", &false);
    }

    /// `BoundLimitNode::Serialize`.
    fn limit_node(s: &mut BinarySerializer, field: u16, tag: &str, node: LimitNode) {
        s.write_object(field, tag, |s| {
            let (ty, integer, percentage) = match node {
                LimitNode::Unset => ("UNSET", 0, -1.0),
                LimitNode::Value(value) => ("CONSTANT_VALUE", value, -1.0),
                LimitNode::Percentage(percentage) => ("CONSTANT_PERCENTAGE", 0, percentage),
                LimitNode::Expression => ("EXPRESSION_VALUE", 0, -1.0),
            };
            s.write_property(100, "type", ty);
            s.write_property_with_default(101, "constant_integer", &integer);
            s.write_property(102, "constant_percentage", &percentage);
            s.write_property_with_default(103, "expression", &W(|s| s.begin_nullable(false), true));
        });
    }

    /// A join of DuckDB's: by its comparisons, or by an arbitrary condition without any.
    fn join(
        &self,
        s: &mut BinarySerializer,
        join_type: JoinType,
        mark_index: usize,
        conditions: &[JoinCondition],
        predicate: Option<&BoundExpr>,
    ) {
        if conditions.is_empty() {
            // `LogicalAnyJoin::Serialize`
            s.write_property(200, "join_type", join_type_name(join_type));
            s.write_property_with_default(201, "mark_index", &(mark_index as u64));
            s.write_property_with_default(202, "left_projection_map", &Vec::<u64>::new());
            s.write_property_with_default(203, "right_projection_map", &Vec::<u64>::new());
            let truth = BoundExpr::constant(Value::boolean(true));
            self.optional_expr_property(s, 204, "condition", Some(predicate.unwrap_or(&truth)));
            return;
        }
        self.comparison_join(s, join_type, mark_index, conditions, None, predicate, &[]);
    }

    /// `LogicalComparisonJoin::Serialize`; `inequality` is the comparison of an AsOf join.
    #[allow(clippy::too_many_arguments, reason = "the fields of a comparison join")]
    fn comparison_join(
        &self,
        s: &mut BinarySerializer,
        join_type: JoinType,
        mark_index: usize,
        conditions: &[JoinCondition],
        inequality: Option<(&BoundExpr, &BoundExpr, ComparisonOp)>,
        predicate: Option<&BoundExpr>,
        duplicates: &[BoundExpr],
    ) {
        s.write_property(200, "join_type", join_type_name(join_type));
        s.write_property_with_default(201, "mark_index", &(mark_index as u64));
        s.write_property_with_default(202, "left_projection_map", &Vec::<u64>::new());
        s.write_property_with_default(203, "right_projection_map", &Vec::<u64>::new());
        let mut all: Vec<(&BoundExpr, &BoundExpr, ComparisonOp)> =
            conditions.iter().map(|c| (&c.left, &c.right, c.comparison)).collect();
        all.extend(inequality);
        let condition = |s: &mut BinarySerializer,
                         (left, right, comparison): &(&BoundExpr, &BoundExpr, ComparisonOp)| {
            s.write_object_value(|s| {
                self.optional_expr_property(s, 100, "left", Some(left));
                self.optional_expr_property(s, 101, "right", Some(right));
                s.write_property(102, "comparison", comparison_names(*comparison).0);
            })
        };
        s.write_property_with_default(204, "conditions", &W(|s| list(s, &all, condition), all.is_empty()));
        let mark_types = if join_type == JoinType::Mark {
            vec![LogicalType::BOOLEAN]
        } else {
            Vec::new()
        };
        s.write_property_with_default(205, "mark_types", &mark_types);
        self.exprs_property(s, 206, "duplicate_eliminated_columns", duplicates);
        s.write_property_with_explicit_default(207, "delim_flipped", &false, &false);
        // the conditions that are not comparisons of the two sides, under the field that had them
        if let Some(predicate) = predicate {
            self.optional_expr_property(s, 208, "predicate", Some(predicate));
        }
    }

    /// `LogicalCopyToFile::Serialize`, without the copy function's bind data.
    fn copy_to_file(&self, s: &mut BinarySerializer, op: &LogicalOperator, path: &str, names: &[String], format: &str) {
        s.write_property(200, "file_path", path);
        s.write_property(201, "use_tmp_file", &false);
        s.write_property(
            202,
            "filename_pattern",
            &W(
                |s| {
                    s.write_object_value(|s| {
                        s.write_property_with_default(200, "base", "data_");
                        s.write_property_with_default(201, "pos", &5u64);
                        s.write_property_with_default(202, "uuid", &false);
                        s.write_property_with_default(203, "segments", &Vec::<u64>::new());
                    })
                },
                false,
            ),
        );
        s.write_property(203, "overwrite_or_ignore", "COPY_ERROR_ON_CONFLICT");
        s.write_property(204, "per_thread_output", &false);
        s.write_property(205, "partition_output", &false);
        s.write_property(206, "partition_columns", &Vec::<u64>::new());
        s.write_property(207, "names", &names.to_vec());
        let types = op.children.first().map(|c| c.types()).unwrap_or_default();
        s.write_property(208, "expected_types", &types);
        // `CopyInfo::Serialize`
        s.write_property(
            209,
            "copy_info",
            &W(
                |s| {
                    s.begin_nullable(true);
                    s.write_object_value(|s| {
                        s.write_property(100, "info_type", "COPY_INFO");
                        s.write_property_with_default(200, "catalog", "");
                        s.write_property_with_default(201, "schema", "");
                        s.write_property_with_default(202, "table", "");
                        s.write_property_with_default(203, "select_list", &Vec::<String>::new());
                        s.write_property_with_default(204, "is_from", &false);
                        s.write_property_with_default(205, "format", format);
                        s.write_property_with_default(206, "file_path", path);
                    })
                },
                false,
            ),
        );
        s.write_property(210, "function_name", format);
        s.write_property(211, "function_has_serialize", &false);
        s.write_property(213, "file_extension", format);
        s.write_property(214, "rotate", &false);
        s.write_property(215, "return_type", "CHANGED_ROWS");
    }

    /// `BoundMergeIntoAction::Serialize`.
    fn merge_action(&self, s: &mut BinarySerializer, action: &crate::planner::logical::BoundMergeAction) {
        s.begin_nullable(true);
        s.write_object_value(|s| {
            let ty = match action.action_type {
                MergeActionType::Update => "MERGE_UPDATE",
                MergeActionType::Delete => "MERGE_DELETE",
                MergeActionType::Insert => "MERGE_INSERT",
                MergeActionType::DoNothing => "MERGE_DO_NOTHING",
                MergeActionType::Error => "MERGE_ERROR",
            };
            s.write_property(200, "action_type", ty);
            self.optional_expr_property(s, 201, "condition", action.condition.as_ref());
            s.write_property_with_default(202, "columns", &u64s(action.columns.iter().copied()));
            self.exprs_property(s, 203, "expressions", &action.expressions);
            s.write_property_with_default(204, "column_index_map", &Vec::<u64>::new());
            s.write_property_with_default(205, "update_is_del_and_insert", &false);
        });
    }

    /// `TableCatalogEntry::GetInfo`, without the table's constraints.
    fn table_info(&self, s: &mut BinarySerializer, table: &TableCatalogEntry) {
        let path = [table.catalog.clone(), table.schema.clone(), table.name.clone()];
        let columns: Vec<ColumnDefinition> = table.columns.clone();
        match create_table_info(&path, &table.name, &columns) {
            Ok(info) => s.write_property_with_default(200, "table_info", &W(|s| ptr(s, &info), false)),
            Err(error) => self.fail(error),
        }
    }
}

/// What a `LOGICAL_GET` has.
struct GetFields<'a> {
    table_index: usize,
    types: Vec<LogicalType>,
    names: Vec<String>,
    column_ids: Vec<usize>,
    projection: Vec<usize>,
    filters: Vec<&'a BoundExpr>,
    zone_filters: Vec<&'a BoundExpr>,
    function: &'a str,
    parameters: Vec<Value>,
    data: Option<&'a dyn Fn(&mut BinarySerializer)>,
    sample: Option<(&'a ast::SampleOptions, f64)>,
}

#[derive(Debug, Clone, Copy)]
enum LimitNode {
    Unset,
    Value(u64),
    Percentage(f64),
    Expression,
}

/// The first column of the input that `expr` reads.
fn first_column(expr: &BoundExpr) -> Option<usize> {
    if let BoundExprKind::Reference(index) = &expr.kind {
        return Some(*index);
    }
    if let BoundExprKind::ColumnRef { binding, .. } = &expr.kind {
        return Some(binding.column_index);
    }
    expr.children().into_iter().find_map(first_column)
}

/// `ColumnIndex::Serialize`: the column itself, or the row id.
fn column_index(s: &mut BinarySerializer, id: usize) {
    let index = match id {
        COLUMN_IDENTIFIER_ROW_ID => u64::MAX,
        COLUMN_IDENTIFIER_ROW_NUMBER => u64::MAX - 1,
        id => id as u64,
    };
    s.write_object_value(|s| {
        s.write_property_with_default(1, "index", &index);
        s.write_property_with_default(2, "child_indexes", &Vec::<u64>::new());
        let full = "FULL_READ".to_string();
        s.write_property_with_explicit_default(3, "index_type", &full, &full);
        s.write_property_with_explicit_default(4, "type", &LogicalType::INVALID, &LogicalType::INVALID);
        s.write_property_with_explicit_default(5, "field", &String::new(), &String::new());
        s.write_property_with_explicit_default(6, "has_index", &true, &true);
    });
}

/// `SampleOptions::Serialize`.
fn sample_options(s: &mut BinarySerializer, options: &ast::SampleOptions, rate: f64) {
    s.write_object_value(|s| {
        let size = if options.is_percentage {
            options.sample_size.parse::<f64>().map(Value::double).ok()
        } else {
            options.sample_size.parse::<i64>().map(Value::bigint).ok()
        };
        s.write_property(
            100,
            "sample_size",
            &size.unwrap_or_else(|| Value::varchar(options.sample_size.clone())),
        );
        s.write_property_with_default(101, "is_percentage", &options.is_percentage);
        let method = match options.method {
            ast::SampleMethod::System => "System",
            ast::SampleMethod::Bernoulli => "Bernoulli",
            ast::SampleMethod::Reservoir => "Reservoir",
        };
        s.write_property(102, "method", method);
        s.write_property_with_default(103, "seed", &options.seed.map_or(-1, |seed| seed as i64));
        s.write_property_with_default(104, "repeatable", &options.repeatable);
        s.write_property_with_explicit_default(105, "sample_rate", &rate, &-1.0);
    });
}

/// A `CreateTableInfo` of `columns`, named by `path`.
fn create_table_info(path: &[String], name: &str, columns: &[ColumnDefinition]) -> Result<g::CreateInfo> {
    let columns = columns
        .iter()
        .map(|column| {
            crate::storage::writer::stored_column(&LogicalColumn {
                name: &column.name,
                ty: &column.ty,
                definition: Some(column),
                generated: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(to_stored::create_info(
        catalog_type::TABLE_ENTRY,
        path.to_vec(),
        g::CreateInfoKind::CreateTableInfo(g::CreateTableInfo {
            table: name.to_string(),
            columns: g::ColumnList { columns },
            constraints: Vec::new(),
            query: None,
            partition_keys: Vec::new(),
            sort_keys: Vec::new(),
            options: Vec::new(),
        }),
    ))
}
