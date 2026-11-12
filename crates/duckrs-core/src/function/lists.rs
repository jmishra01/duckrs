// Ported from duckdb/src/function/scalar/list/*.cpp and
// duckdb/extension/core_functions/scalar/list/*.cpp @ rust-port-baseline
//
//! The list functions, row by row over `Value`s until Phase 13.

use super::BuiltinFunctions;
use super::aggregates::value_keys;
use super::binder::{bind_aggregate_function, expression_return_type};
use super::bit_functions::strict_rows;
use super::combine_types::force_max_logical_type;
use super::nested::{bound_function, row_wise};
use crate::execution::expression_executor::evaluate_scalar;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use std::collections::HashSet;

/// `list_sum` and the like: `list_aggr` of the aggregate after the prefix.
fn rewritten_aggregate(name: &str) -> Option<&'static str> {
    Some(match name {
        "list_sum" => "sum",
        "list_min" => "min",
        "list_max" => "max",
        "list_avg" => "avg",
        "list_count" => "count",
        "list_first" => "first",
        "list_last" => "last",
        "list_any_value" => "any_value",
        "list_mode" => "mode",
        "list_median" => "median",
        "list_string_agg" => "string_agg",
        "list_product" => "product",
        "list_bool_and" => "bool_and",
        "list_bool_or" => "bool_or",
        "list_stddev_samp" => "stddev_samp",
        "list_stddev_pop" => "stddev_pop",
        "list_var_samp" => "var_samp",
        "list_var_pop" => "var_pop",
        "list_mad" => "mad",
        "list_entropy" => "entropy",
        "list_histogram" => "histogram",
        "list_bit_and" => "bit_and",
        "list_bit_or" => "bit_or",
        "list_bit_xor" => "bit_xor",
        _ => return None,
    })
}

/// Whether [`bind_list_function`] binds `name`.
pub fn is_list_function(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    rewritten_aggregate(&lower).is_some()
        || matches!(
            lower.as_str(),
            "list_aggr"
                | "list_aggregate"
                | "array_aggr"
                | "array_aggregate"
                | "aggregate"
                | "list_contains"
                | "list_has"
                | "array_contains"
                | "array_has"
                | "list_position"
                | "list_indexof"
                | "array_position"
                | "array_indexof"
                | "list_distinct"
                | "array_distinct"
                | "list_unique"
                | "array_unique"
                | "list_reverse"
                | "array_reverse"
                | "list_has_any"
                | "array_has_any"
                | "list_has_all"
                | "array_has_all"
                | "&&"
                | "@>"
                | "<@"
                | "flatten"
                | "list_resize"
                | "array_resize"
                | "list_zip"
                | "array_zip"
                | "list_select"
                | "array_select"
                | "list_where"
                | "array_where"
                | "list_grade_up"
                | "array_grade_up"
                | "grade_up"
        )
}

pub(super) fn no_match(name: &str, children: &[BoundExpr]) -> Error {
    let types: Vec<String> = children
        .iter()
        .map(|c| super::binder::expression_return_type(c).to_string())
        .collect();
    Error::new(
        ErrorKind::Binder,
        format!(
            "No function matches the given name and argument types '{name}({})'. You might need to add explicit type \
             casts.",
            types.join(", ")
        ),
    )
}

/// A LIST of what `child` holds, which also takes ARRAYs and NULLs.
fn as_list(child: BoundExpr) -> BoundExpr {
    match child.return_type.id() {
        LogicalTypeId::Array => {
            let element = child.return_type.child_type().expect("a child type").clone();
            child.cast_to(&LogicalType::list(element), false)
        }
        LogicalTypeId::SqlNull => child.cast_to(&LogicalType::list(LogicalType::SQLNULL), false),
        _ => child,
    }
}

fn element_of(list: &BoundExpr) -> Option<LogicalType> {
    (list.return_type.id() == LogicalTypeId::List).then(|| list.return_type.child_type().expect("a child").clone())
}

fn items(value: &Value) -> &[Value] {
    match value.data() {
        ValueData::List(items) => items,
        _ => &[],
    }
}

fn keys_of(values: &[Value], ty: &LogicalType) -> Result<Vec<Vec<u8>>> {
    let mut vector = Vector::new(ty.clone(), values.len().max(1));
    for (row, value) in values.iter().enumerate() {
        vector.set_value(row, value)?;
    }
    value_keys(&vector, values.len())
}

/// `ListSearchSimpleOp`: where the needle is among the elements. A NULL needle matches a NULL
/// element only.
fn search(values: &[Value], needle: &Value) -> Result<Option<usize>> {
    let ty = needle.logical_type();
    let keys = keys_of(values, ty)?;
    let target = keys_of(std::slice::from_ref(needle), ty)?;
    Ok(values
        .iter()
        .zip(&keys)
        .position(|(value, key)| value.is_null() == needle.is_null() && *key == target[0]))
}

fn list_contains(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::BOOLEAN));
        }
        let values = items(&row[0]);
        // an empty list never contains anything, not even NULL
        if values.is_empty() {
            return Ok(Value::boolean(false));
        }
        if row[1].is_null() {
            return Ok(Value::null(LogicalType::BOOLEAN));
        }
        Ok(Value::boolean(search(values, &row[1])?.is_some()))
    })
}

fn list_position(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(LogicalType::INTEGER));
        }
        Ok(match search(items(&row[0]), &row[1])? {
            Some(at) => Value::integer(at as i32 + 1),
            None => Value::null(LogicalType::INTEGER),
        })
    })
}

fn distinct_items(row: &Value) -> Result<Vec<Value>> {
    let values = items(row);
    let ty = row.logical_type().child_type().expect("a list").clone();
    let keys = keys_of(values, &ty)?;
    let mut seen = HashSet::new();
    Ok(values
        .iter()
        .zip(&keys)
        .filter(|(value, key)| !value.is_null() && seen.insert(*key))
        .map(|(value, _)| value.clone())
        .collect())
}

fn list_distinct(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        Value::new(ty.clone(), ValueData::List(distinct_items(&row[0])?))
    })
}

fn list_unique(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::ubigint(distinct_items(&row[0])?.len() as u64))
    })
}

fn list_reverse(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        Value::new(
            ty.clone(),
            ValueData::List(items(&row[0]).iter().rev().cloned().collect()),
        )
    })
}

fn key_set(row: &Value) -> Result<HashSet<Vec<u8>>> {
    let values = items(row);
    let ty = row.logical_type().child_type().expect("a list").clone();
    let keys = keys_of(values, &ty)?;
    Ok(values
        .iter()
        .zip(keys)
        .filter(|(value, _)| !value.is_null())
        .map(|(_, key)| key)
        .collect())
}

fn list_has_any(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::boolean(!key_set(&row[0])?.is_disjoint(&key_set(&row[1])?)))
    })
}

fn list_has_all(args: &DataChunk, result: &mut Vector) -> Result<()> {
    strict_rows(args, result, |row| {
        Ok(Value::boolean(key_set(&row[1])?.is_subset(&key_set(&row[0])?)))
    })
}

fn flatten(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let flat = items(&row[0])
            .iter()
            .flat_map(|inner| items(inner).iter().cloned())
            .collect();
        Value::new(ty.clone(), ValueData::List(flat))
    })
}

fn list_resize(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let element = ty.child_type().expect("a list").clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let Some(size) = row[1].as_i128() else {
            return Value::new(ty.clone(), ValueData::List(items(&row[0]).to_vec()));
        };
        if size < 0 {
            return Err(Error::new(ErrorKind::OutOfRange, "Cannot resize list to negative size"));
        }
        let fill = row.get(2).cloned().unwrap_or_else(|| Value::null(element.clone()));
        let mut values = items(&row[0]).to_vec();
        values.resize(size as usize, fill);
        Value::new(ty.clone(), ValueData::List(values))
    })
}

fn list_zip(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let entry = ty.child_type().expect("a list").clone();
    let children = entry.struct_children().to_vec();
    row_wise(args, result, |row| {
        let lists = &row[..children.len()];
        let longest = lists.iter().map(|l| items(l).len()).max().unwrap_or(0);
        let shortest = lists.iter().map(|l| items(l).len()).min().unwrap_or(0);
        let truncate = matches!(row.get(children.len()).map(Value::data), Some(ValueData::Bool(true)));
        let size = if truncate { shortest } else { longest };
        let mut entries = Vec::with_capacity(size);
        for at in 0..size {
            let fields = lists
                .iter()
                .zip(&children)
                .map(|(list, (_, ty))| items(list).get(at).cloned().unwrap_or_else(|| Value::null(ty.clone())))
                .collect();
            entries.push(Value::new(entry.clone(), ValueData::Struct(fields))?);
        }
        Value::new(ty.clone(), ValueData::List(entries))
    })
}

fn null_selection() -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "NULLs are not allowed as list elements in the second input parameter.",
    )
}

fn list_select(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let element = ty.child_type().expect("a list").clone();
    strict_rows(args, result, |row| {
        let values = items(&row[0]);
        let mut picked = Vec::new();
        for index in items(&row[1]) {
            let Some(index) = index.as_i128() else {
                return Err(null_selection());
            };
            let at = usize::try_from(index - 1).ok();
            picked.push(
                at.and_then(|at| values.get(at).cloned())
                    .unwrap_or_else(|| Value::null(element.clone())),
            );
        }
        Value::new(ty.clone(), ValueData::List(picked))
    })
}

fn list_where(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let (values, mask) = (items(&row[0]), items(&row[1]));
        if mask.iter().any(Value::is_null) {
            return Err(null_selection());
        }
        // `SetSelectionVectorWhere`: a TRUE past the end of the list selects a NULL
        let element = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
        let picked = mask
            .iter()
            .enumerate()
            .filter(|(_, keep)| matches!(keep.data(), ValueData::Bool(true)))
            .map(|(at, _)| values.get(at).cloned().unwrap_or_else(|| Value::null(element.clone())))
            .collect();
        Value::new(ty.clone(), ValueData::List(picked))
    })
}

fn list_grade_up(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    strict_rows(args, result, |row| {
        let values = items(&row[0]);
        let descending = row.get(1).is_some_and(|o| {
            o.try_to_string()
                .is_ok_and(|text| text.to_uppercase().starts_with("DESC"))
        });
        let element = row[0].logical_type().child_type().expect("a list").clone();
        let keys = keys_of(values, &element)?;
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by(|a, b| {
            let (left, right) = (values[*a].is_null(), values[*b].is_null());
            if left || right {
                return right.cmp(&left);
            }
            if descending {
                keys[*b].cmp(&keys[*a])
            } else {
                keys[*a].cmp(&keys[*b])
            }
        });
        let grades = order.into_iter().map(|at| Value::bigint(at as i64 + 1)).collect();
        Value::new(ty.clone(), ValueData::List(grades))
    })
}

/// `ListAggregatesFunction`: the aggregate over the items of each list.
fn list_aggr(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let name = row[1].try_to_string()?;
        let set = BuiltinFunctions::get().aggregate(&name).expect("checked when binding");
        let element = row[0].logical_type().child_type().expect("a list").clone();
        let mut children: Vec<BoundExpr> = vec![BoundExpr::constant(Value::null(element))];
        children.extend(row[2..].iter().map(|extra| BoundExpr::constant(extra.clone())));
        let bound = bind_aggregate_function(set, children, None, false)?;
        let values = items(&row[0]);
        let input_type = bound.children[0].return_type.clone();
        let mut vector = Vector::new(input_type.clone(), values.len().max(1));
        for (at, value) in values.iter().enumerate() {
            let value = if row.len() > 2 {
                let mut fields = vec![cast_value(value, &input_type.struct_children()[0].1)?];
                fields.extend(row[2..].iter().cloned());
                Value::new(input_type.clone(), ValueData::Struct(fields))?
            } else {
                cast_value(value, &input_type)?
            };
            vector.set_value(at, &value)?;
        }
        let selected: Vec<(usize, usize)> = (0..values.len()).map(|at| (at, 0)).collect();
        let mut states = [bound.function.kind.initial_state()];
        if !selected.is_empty() {
            bound
                .function
                .update(Some(&vector), values.len(), &selected, &mut states)?;
        }
        let input = Some(&input_type);
        bound.function.finalize(&states[0], &bound.return_type, input)
    })
}

fn cast_value(value: &Value, ty: &LogicalType) -> Result<Value> {
    if value.logical_type() == ty {
        return Ok(value.clone());
    }
    evaluate_scalar(&BoundExpr::constant(value.clone()).cast_to(ty, false))
}

fn bind_list_aggr(name: &str, aggregate: Option<&str>, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let mut children = children;
    if let Some(aggregate) = aggregate {
        if children.is_empty() {
            return Err(Error::new(
                ErrorKind::Binder,
                format!("Macro {name}() does not support the supplied arguments."),
            ));
        }
        children.insert(1, BoundExpr::constant(Value::varchar(aggregate)));
    }
    if children.len() < 2 {
        return Err(no_match(name, &children));
    }
    let list = as_list(children.remove(0));
    let Some(element) = element_of(&list) else {
        return Err(no_match(name, &[list]));
    };
    let which = children.remove(0).cast_to(&LogicalType::VARCHAR, false);
    if !which.is_foldable() {
        return Err(Error::new(
            ErrorKind::Binder,
            "Aggregate function name must be a constant",
        ));
    }
    let function = evaluate_scalar(&which)?.try_to_string()?;
    let Some(set) = BuiltinFunctions::get().aggregate(&function) else {
        return Err(Error::new(
            ErrorKind::Catalog,
            format!("Aggregate Function with name {function} does not exist!"),
        ));
    };
    if children.iter().any(|child| !child.is_foldable()) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Aggregate function {function} is not supported for list_aggr: extra arguments must be constant"),
        ));
    }
    let mut probe = vec![BoundExpr::constant(Value::null(element))];
    probe.extend(children.iter().cloned());
    let return_type = bind_aggregate_function(set, probe, None, false)?.return_type;
    let mut arguments = vec![list, which];
    arguments.extend(children);
    Ok(bound_function(name, arguments, return_type, list_aggr))
}

/// `list_contains` and the like, whose needle casts to the elements or the other way around.
fn bind_search(
    name: &str,
    list: BoundExpr,
    needle: BoundExpr,
    return_type: LogicalType,
    callback: super::ScalarCallback,
) -> Result<BoundExpr> {
    // a string constant has to cast to a list, like any other argument of the template LIST(T)
    let list = if matches!(&list.kind, BoundExprKind::Constant(_)) && list.return_type.id() == LogicalTypeId::Varchar {
        list.cast_to(&LogicalType::list(needle.return_type.clone()), false)
    } else {
        as_list(list)
    };
    let Some(element) = element_of(&list) else {
        return Err(no_match(name, &[list, needle]));
    };
    // a string literal takes the type of the elements, and fails to cast to it if it has to
    let needle_type = match expression_return_type(&needle) {
        literal if literal.id() == LogicalTypeId::StringLiteral && element.id() != LogicalTypeId::SqlNull => {
            element.clone()
        }
        _ => needle.return_type.clone(),
    };
    let common = match force_max_logical_type(&element, &needle_type) {
        common if common.id() == LogicalTypeId::SqlNull => LogicalType::INTEGER,
        common => common,
    };
    let list = list.cast_to(&LogicalType::list(common.clone()), false);
    let needle = needle.cast_to(&common, false);
    Ok(bound_function(name, vec![list, needle], return_type, callback))
}

/// Binds a function of [`is_list_function`].
pub fn bind_list_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let lower = name.to_ascii_lowercase();
    if let Some(aggregate) = rewritten_aggregate(&lower) {
        return bind_list_aggr(&lower, Some(aggregate), children);
    }
    let list_arg = |children: &[BoundExpr]| -> Result<(BoundExpr, LogicalType)> {
        let Some(first) = children.first() else {
            return Err(no_match(&lower, children));
        };
        let list = as_list(first.clone());
        let element = element_of(&list).ok_or_else(|| no_match(&lower, children))?;
        Ok((list, element))
    };
    match lower.as_str() {
        "list_aggr" | "list_aggregate" | "array_aggr" | "array_aggregate" | "aggregate" => {
            bind_list_aggr("list_aggr", None, children)
        }
        "list_contains" | "list_has" | "array_contains" | "array_has" => {
            let [list, needle] = <[BoundExpr; 2]>::try_from(children).map_err(|c| no_match(&lower, &c))?;
            bind_search("list_contains", list, needle, LogicalType::BOOLEAN, list_contains)
        }
        "list_position" | "list_indexof" | "array_position" | "array_indexof" => {
            let [list, needle] = <[BoundExpr; 2]>::try_from(children).map_err(|c| no_match(&lower, &c))?;
            bind_search("list_position", list, needle, LogicalType::INTEGER, list_position)
        }
        "list_distinct" | "array_distinct" => {
            if children.len() != 1 {
                return Err(no_match(&lower, &children));
            }
            let (list, _) = list_arg(&children)?;
            let ty = list.return_type.clone();
            Ok(bound_function("list_distinct", vec![list], ty, list_distinct))
        }
        "list_unique" | "array_unique" => {
            if children.len() != 1 {
                return Err(no_match(&lower, &children));
            }
            let (list, _) = list_arg(&children)?;
            Ok(bound_function(
                "list_unique",
                vec![list],
                LogicalType::UBIGINT,
                list_unique,
            ))
        }
        "list_reverse" | "array_reverse" => {
            if children.len() != 1 {
                return Err(no_match(&lower, &children));
            }
            let (list, _) = list_arg(&children)?;
            let ty = list.return_type.clone();
            Ok(bound_function("list_reverse", vec![list], ty, list_reverse))
        }
        "list_has_any" | "array_has_any" | "list_has_all" | "array_has_all" | "&&" | "@>" | "<@" => {
            // `&&` is also `ST_Intersects_Extent`
            if lower == "&&"
                && let [left, right] = children.as_slice()
                && [left, right]
                    .iter()
                    .any(|c| c.return_type.id() == LogicalTypeId::Geometry)
                && [left, right].iter().all(|c| {
                    matches!(
                        super::binder::expression_return_type(c).id(),
                        LogicalTypeId::Geometry | LogicalTypeId::StringLiteral
                    )
                })
            {
                let geometry = LogicalType::new(LogicalTypeId::Geometry);
                return Ok(super::geometry::bind_intersects_extent(
                    left.clone().cast_to(&geometry, false),
                    right.clone().cast_to(&geometry, false),
                ));
            }
            let (left, right) = match children.as_slice() {
                // `a <@ b` is whether b has all of a
                [left, right] if lower == "<@" => (as_list(right.clone()), as_list(left.clone())),
                [left, right] => (as_list(left.clone()), as_list(right.clone())),
                _ => return Err(no_match(&lower, &children)),
            };
            let (Some(a), Some(b)) = (element_of(&left), element_of(&right)) else {
                return Err(no_match(&lower, &children));
            };
            let common = LogicalType::list(force_max_logical_type(&a, &b));
            let args = vec![left.cast_to(&common, false), right.cast_to(&common, false)];
            if lower.ends_with("any") || lower == "&&" {
                Ok(bound_function("list_has_any", args, LogicalType::BOOLEAN, list_has_any))
            } else {
                Ok(bound_function("list_has_all", args, LogicalType::BOOLEAN, list_has_all))
            }
        }
        "flatten" => {
            if children
                .first()
                .is_some_and(|c| c.return_type.id() == LogicalTypeId::SqlNull)
            {
                return Ok(BoundExpr::constant(Value::sql_null()));
            }
            let (list, element) = list_arg(&children)?;
            if element.id() == LogicalTypeId::SqlNull {
                let ty = LogicalType::list(LogicalType::SQLNULL);
                return Ok(bound_function("flatten", vec![list], ty, flatten));
            }
            let inner = as_list(BoundExpr::constant(Value::null(element.clone())));
            if element_of(&inner).is_none() {
                return Err(no_match(&lower, &children));
            }
            let ty = inner.return_type.clone();
            let list = list.cast_to(&LogicalType::list(ty.clone()), false);
            Ok(bound_function("flatten", vec![list], ty, flatten))
        }
        "list_resize" | "array_resize" => {
            // a NULL list resizes to NULL, whatever the fill
            if children
                .first()
                .is_some_and(|c| c.return_type.id() == LogicalTypeId::SqlNull)
                && (2..=3).contains(&children.len())
            {
                return Ok(BoundExpr::constant(Value::sql_null()));
            }
            if let Some(first) = children.first()
                && !matches!(
                    first.return_type.id(),
                    LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Unknown
                )
            {
                return Err(Error::new(
                    ErrorKind::Binder,
                    format!("\"{lower}\" can only operate on LISTs"),
                ));
            }
            let (list, element) = list_arg(&children)?;
            if !(2..=3).contains(&children.len()) {
                return Err(no_match(&lower, &children));
            }
            let mut args = vec![list.clone(), children[1].clone().cast_to(&LogicalType::UBIGINT, false)];
            if let Some(fill) = children.get(2) {
                args.push(fill.clone().cast_to(&element, false));
            }
            Ok(bound_function("list_resize", args, list.return_type, list_resize))
        }
        "list_zip" | "array_zip" => {
            let mut children = children;
            let truncate = match children.last() {
                Some(last) if last.return_type.id() == LogicalTypeId::Boolean => children.pop(),
                _ => None,
            };
            let mut args = Vec::with_capacity(children.len() + 1);
            let mut fields = Vec::with_capacity(children.len());
            for child in children {
                let list = as_list(child);
                let element = element_of(&list).ok_or_else(|| no_match(&lower, &[]))?;
                fields.push(element);
                args.push(list);
            }
            if args.is_empty() {
                return Err(no_match(&lower, &args));
            }
            args.extend(truncate);
            let ty = LogicalType::list(LogicalType::tuple(fields));
            Ok(bound_function("list_zip", args, ty, list_zip))
        }
        "list_select" | "array_select" => {
            let (list, _) = list_arg(&children)?;
            let [_, indexes] = children.as_slice() else {
                return Err(no_match(&lower, &children));
            };
            let indexes = as_list(indexes.clone()).cast_to(&LogicalType::list(LogicalType::BIGINT), false);
            let ty = list.return_type.clone();
            Ok(bound_function("list_select", vec![list, indexes], ty, list_select))
        }
        "list_where" | "array_where" => {
            let (list, _) = list_arg(&children)?;
            let [_, mask] = children.as_slice() else {
                return Err(no_match(&lower, &children));
            };
            let mask = as_list(mask.clone()).cast_to(&LogicalType::list(LogicalType::BOOLEAN), false);
            let ty = list.return_type.clone();
            Ok(bound_function("list_where", vec![list, mask], ty, list_where))
        }
        "list_grade_up" | "array_grade_up" | "grade_up" => {
            let (list, _) = list_arg(&children)?;
            let mut args = vec![list];
            args.extend(
                children[1..]
                    .iter()
                    .map(|c| c.clone().cast_to(&LogicalType::VARCHAR, false)),
            );
            let ty = LogicalType::list(LogicalType::BIGINT);
            Ok(bound_function("list_grade_up", args, ty, list_grade_up))
        }
        other => unreachable!("{other} is not a list function"),
    }
}
