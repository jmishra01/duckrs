// Ported from duckdb/src/execution/expression_executor.cpp and
// duckdb/src/execution/expression_executor/execute_{cast,comparison,conjunction,constant,
// function,operator,reference}.cpp @ rust-port-baseline
//
//! Evaluates bound expressions over a chunk of rows, a vector at a time. Filters evaluate
//! their condition and keep the rows where it is true (DuckDB's specialized `Select` paths
//! are a later optimization).

use crate::function::cast::{CastMode, cast_vector};
use crate::planner::expression::{BoundExpr, BoundExprKind, BoundOperator};
use duckrs_common::comparison::{ComparisonType, DuckOrd};
use duckrs_common::executor;
use duckrs_common::types::{LogicalTypeId, NativeType, PhysicalType};
use duckrs_common::value::Value;
use duckrs_common::vector::{DataChunk, SelectionVector, Vector, VectorType};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::ComparisonOp;

fn comparison_type(op: ComparisonOp) -> ComparisonType {
    match op {
        ComparisonOp::Equal => ComparisonType::Equal,
        ComparisonOp::NotEqual => ComparisonType::NotEqual,
        ComparisonOp::LessThan => ComparisonType::LessThan,
        ComparisonOp::GreaterThan => ComparisonType::GreaterThan,
        ComparisonOp::LessThanOrEqual => ComparisonType::LessThanOrEqual,
        ComparisonOp::GreaterThanOrEqual => ComparisonType::GreaterThanOrEqual,
        ComparisonOp::DistinctFrom => ComparisonType::DistinctFrom,
        ComparisonOp::NotDistinctFrom => ComparisonType::NotDistinctFrom,
    }
}

/// Whether both inputs are constant, so the result is a single constant row.
fn all_constant(vectors: &[&Vector]) -> bool {
    vectors.iter().all(|v| v.vector_type() == VectorType::Constant)
}

/// A comparison of two vectors of the same type, row by row. DISTINCT FROM compares NULLs
/// too; the other comparisons are NULL when either side is.
fn compare_typed<T: NativeType + DuckOrd>(
    left: &Vector,
    right: &Vector,
    result: &mut Vector,
    count: usize,
    op: ComparisonType,
) {
    let null_aware = matches!(op, ComparisonType::DistinctFrom | ComparisonType::NotDistinctFrom);
    if !null_aware {
        executor::binary::<T, T, bool>(left, right, result, count, |a, b| {
            op.evaluate(false, false, || a.duck_cmp(&b))
        });
        return;
    }
    let rows = if all_constant(&[left, right]) { 1 } else { count };
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    let (ld, rd) = (lu.data::<T>(), ru.data::<T>());
    result.prepare_flat(rows);
    let out = result.data_mut::<bool>();
    for (i, slot) in out.iter_mut().enumerate().take(rows) {
        let (li, ri) = (lu.sel.get(i), ru.sel.get(i));
        let (lnull, rnull) = (!lu.validity.is_valid(li), !ru.validity.is_valid(ri));
        *slot = op.evaluate(lnull, rnull, || ld[li].duck_cmp(&rd[ri]));
    }
    if rows == 1 && count != 1 {
        result.set_constant();
    }
}

fn compare_strings(left: &Vector, right: &Vector, result: &mut Vector, count: usize, op: ComparisonType) {
    let null_aware = matches!(op, ComparisonType::DistinctFrom | ComparisonType::NotDistinctFrom);
    if !null_aware {
        executor::binary_str::<bool>(left, right, result, count, |a, b| {
            op.evaluate(false, false, || a.cmp(b))
        });
        return;
    }
    let rows = if all_constant(&[left, right]) { 1 } else { count };
    let (lu, ru) = (left.to_unified(count), right.to_unified(count));
    result.prepare_flat(rows);
    let out = result.data_mut::<bool>();
    for (i, slot) in out.iter_mut().enumerate().take(rows) {
        let (li, ri) = (lu.sel.get(i), ru.sel.get(i));
        let (lnull, rnull) = (!lu.validity.is_valid(li), !ru.validity.is_valid(ri));
        *slot = op.evaluate(lnull, rnull, || lu.str_at(i).cmp(ru.str_at(i)));
    }
    if rows == 1 && count != 1 {
        result.set_constant();
    }
}

fn compare(left: &Vector, right: &Vector, result: &mut Vector, count: usize, op: ComparisonType) -> Result<()> {
    match left.physical_type() {
        PhysicalType::Bool => compare_typed::<bool>(left, right, result, count, op),
        PhysicalType::Int8 => compare_typed::<i8>(left, right, result, count, op),
        PhysicalType::Int16 => compare_typed::<i16>(left, right, result, count, op),
        PhysicalType::Int32 => compare_typed::<i32>(left, right, result, count, op),
        PhysicalType::Int64 => compare_typed::<i64>(left, right, result, count, op),
        PhysicalType::Int128 => compare_typed::<i128>(left, right, result, count, op),
        PhysicalType::UInt8 => compare_typed::<u8>(left, right, result, count, op),
        PhysicalType::UInt16 => compare_typed::<u16>(left, right, result, count, op),
        PhysicalType::UInt32 => compare_typed::<u32>(left, right, result, count, op),
        PhysicalType::UInt64 => compare_typed::<u64>(left, right, result, count, op),
        PhysicalType::UInt128 => compare_typed::<u128>(left, right, result, count, op),
        PhysicalType::Float => compare_typed::<f32>(left, right, result, count, op),
        PhysicalType::Double => compare_typed::<f64>(left, right, result, count, op),
        PhysicalType::Interval => compare_typed::<duckrs_common::types::Interval>(left, right, result, count, op),
        PhysicalType::Varchar => compare_strings(left, right, result, count, op),
        other => {
            return Err(crate::unported(format!("comparisons of {other} values")));
        }
    }
    Ok(())
}

/// Three-valued AND/OR over BOOLEAN vectors (`ExecuteConjunction`).
fn conjunction(children: &[Vector], and: bool, count: usize) -> Vector {
    let refs: Vec<&Vector> = children.iter().collect();
    let rows = if all_constant(&refs) { 1 } else { count };
    let unified: Vec<_> = children.iter().map(|c| c.to_unified(count)).collect();
    let mut result = Vector::new(duckrs_common::types::LogicalType::BOOLEAN, rows.max(1));
    let mut nulls = Vec::new();
    {
        let out = result.data_mut::<bool>();
        for (i, slot) in out.iter_mut().enumerate().take(rows) {
            // AND: false wins over NULL, NULL over true; OR: true wins over NULL, NULL over false
            let mut saw_null = false;
            let mut decided = false;
            for u in &unified {
                let idx = u.sel.get(i);
                if !u.validity.is_valid(idx) {
                    saw_null = true;
                } else if u.data::<bool>()[idx] != and {
                    decided = true;
                    break;
                }
            }
            if decided {
                *slot = !and;
            } else if saw_null {
                nulls.push(i);
            } else {
                *slot = and;
            }
        }
    }
    nulls.into_iter().for_each(|i| result.set_null(i, true));
    if rows == 1 && count != 1 {
        result.set_constant();
    }
    result
}

/// `IS NULL` / `IS NOT NULL`: never NULL themselves.
fn null_check(input: &Vector, count: usize, is_null: bool) -> Vector {
    let constant = input.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    let u = input.to_unified(count);
    let mut result = Vector::new(duckrs_common::types::LogicalType::BOOLEAN, rows.max(1));
    {
        let out = result.data_mut::<bool>();
        for (i, slot) in out.iter_mut().enumerate().take(rows) {
            *slot = u.validity.is_valid(u.sel.get(i)) != is_null;
        }
    }
    if constant && count != 1 {
        result.set_constant();
    }
    result
}

/// `ExpressionExecutor::Execute`: the expression's value for each of the `input` rows.
pub fn execute(expr: &BoundExpr, input: &DataChunk) -> Result<Vector> {
    let count = input.len();
    match &expr.kind {
        BoundExprKind::Constant(value) => Ok(Vector::constant(value)),
        BoundExprKind::Reference(index) => Ok(input.column(*index).clone()),
        BoundExprKind::ColumnRef { .. } => Err(Error::new(
            ErrorKind::Internal,
            "Attempted to execute an unresolved column reference",
        )),
        BoundExprKind::Cast { child, try_cast } => {
            let source = execute(child, input)?;
            let mut result = Vector::new(expr.return_type.clone(), count.max(1));
            let mode = if *try_cast { CastMode::Try } else { CastMode::Strict };
            cast_vector(&source, &mut result, count, mode)?;
            Ok(result)
        }
        BoundExprKind::Comparison { op, left, right } => {
            let (l, r) = (execute(left, input)?, execute(right, input)?);
            if l.logical_type().id() == LogicalTypeId::SqlNull {
                // comparing two untyped NULLs
                let op = comparison_type(*op);
                let value = match op {
                    ComparisonType::DistinctFrom => Value::boolean(false),
                    ComparisonType::NotDistinctFrom => Value::boolean(true),
                    _ => Value::null(duckrs_common::types::LogicalType::BOOLEAN),
                };
                return Ok(Vector::constant(&value));
            }
            let mut result = Vector::new(expr.return_type.clone(), count.max(1));
            compare(&l, &r, &mut result, count, comparison_type(*op))?;
            Ok(result)
        }
        BoundExprKind::Conjunction { and, children } => {
            let children = children.iter().map(|c| execute(c, input)).collect::<Result<Vec<_>>>()?;
            Ok(conjunction(&children, *and, count))
        }
        BoundExprKind::Operator { op, children } => {
            let child = execute(&children[0], input)?;
            Ok(match op {
                BoundOperator::Not => {
                    let mut result = Vector::new(expr.return_type.clone(), count.max(1));
                    executor::unary::<bool, bool>(&child, &mut result, count, |b| !b);
                    result
                }
                BoundOperator::IsNull => null_check(&child, count, true),
                BoundOperator::IsNotNull => null_check(&child, count, false),
            })
        }
        BoundExprKind::Function { function, children, .. } => {
            let arguments = children.iter().map(|c| execute(c, input)).collect::<Result<Vec<_>>>()?;
            let arguments = DataChunk::from_columns(arguments, count);
            let mut result = Vector::new(expr.return_type.clone(), count.max(1));
            (function.function)(&arguments, &mut result)?;
            Ok(result)
        }
    }
}

/// `ExpressionExecutor::Select`: the rows of `input` for which a BOOLEAN expression is true.
pub fn select(expr: &BoundExpr, input: &DataChunk) -> Result<(SelectionVector, usize)> {
    let result = execute(expr, input)?;
    Ok(executor::select::<bool>(&result, input.len(), |b| b))
}

/// `ExpressionExecutor::TryEvaluateScalar`: the value of an expression without columns, or
/// `None` if evaluating it fails.
pub fn try_evaluate_scalar(expr: &BoundExpr) -> Option<Value> {
    evaluate_scalar(expr).ok()
}

/// `ExpressionExecutor::EvaluateScalar`.
pub fn evaluate_scalar(expr: &BoundExpr) -> Result<Value> {
    let input = DataChunk::from_columns(Vec::new(), 1);
    let result = execute(expr, &input)?;
    Ok(result.get_value(0))
}
