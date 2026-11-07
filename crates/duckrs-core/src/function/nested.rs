// Ported from duckdb/extension/core_functions/scalar/{list/list_value,array/array_value,map/map}.cpp
// and duckdb/src/function/scalar/{struct/struct_pack,struct/struct_extract,list/list_extract}.cpp
// @ rust-port-baseline
//
//! The functions that build nested values and take them apart. Their types depend on their
//! arguments, so the binder hands them to [`bind_nested_function`] instead of resolving an
//! overload. They run row by row over [`Value`]s.

use super::ScalarFunction;
use super::binder::expression_return_type;
use super::combine_types::{normalize_type, try_max_logical_type};
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::string_util::{candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::{LogicalType, LogicalTypeId, PhysicalType};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{DataChunk, Vector};
use duckrs_common::{Error, ErrorKind, Result};
use std::sync::Arc;

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message)
}

fn invalid_input(message: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

/// Whether [`bind_nested_function`] binds the function `name`.
pub fn is_nested_function(name: &str) -> bool {
    super::json::is_json_function(name)
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "list_value"
                | "list_pack"
                | "unpivot_list"
                | "array_value"
                | "row"
                | "struct_pack"
                | "map"
                | "struct_extract"
                | "struct_extract_at"
                | "list_extract"
                | "list_element"
                | "array_extract"
                | "union_extract"
                | "union_value"
                | "union_tag"
                | "enum_first"
                | "enum_last"
                | "enum_range"
                | "enum_code"
                | "enum_range_boundary"
        )
}

/// Runs `f` on the arguments of each row.
pub(super) fn row_wise(args: &DataChunk, result: &mut Vector, f: impl Fn(&[Value]) -> Result<Value>) -> Result<()> {
    let count = args.len();
    result.prepare_flat(count.max(1));
    let mut row = Vec::with_capacity(args.column_count());
    for i in 0..count {
        row.clear();
        row.extend(args.columns().iter().map(|c| c.get_value(i)));
        let value = f(&row)?;
        result.set_value(i, &value)?;
    }
    Ok(())
}

fn list_value(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        Value::new(ty.clone(), ValueData::List(row.to_vec()))
    })
}

fn struct_pack(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        Value::new(ty.clone(), ValueData::Struct(row.to_vec()))
    })
}

/// `MapFunction`: the keys and values of a row pairwise, as its entries.
fn map_function(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    let entry = ty.child_type().expect("MAP has an entry type").clone();
    row_wise(args, result, |row| {
        let [keys, values] = row else {
            return Value::new(ty.clone(), ValueData::List(Vec::new()));
        };
        if keys.is_null() || values.is_null() {
            return Ok(Value::null(ty.clone()));
        }
        let (ValueData::List(keys), ValueData::List(values)) = (keys.data(), values.data()) else {
            unreachable!("map takes lists");
        };
        if keys.len() != values.len() {
            return Err(invalid_input(
                "The map key list does not align with the map value list.",
            ));
        }
        let mut entries = Vec::with_capacity(keys.len());
        for (i, (key, value)) in keys.iter().zip(values).enumerate() {
            if key.is_null() {
                return Err(invalid_input("Map keys can not be NULL."));
            }
            if keys[..i].contains(key) {
                return Err(invalid_input("Map keys must be unique."));
            }
            entries.push(Value::new(
                entry.clone(),
                ValueData::Struct(vec![key.clone(), value.clone()]),
            )?);
        }
        Value::new(ty.clone(), ValueData::List(entries))
    })
}

/// `struct_extract` with the (0-based) field index as the second argument.
fn struct_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let index = row[1].as_i128().expect("a field index") as usize;
        Ok(match row[0].data() {
            ValueData::Struct(fields) => fields[index].clone(),
            _ => Value::null(ty.clone()),
        })
    })
}

/// `union_value`: a UNION of one member.
fn union_value(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        Value::new(ty.clone(), ValueData::Struct(vec![Value::utinyint(0), row[0].clone()]))
    })
}

/// `union_extract` with the (0-based) member index as the second argument: NULL unless the tag
/// selects that member.
fn union_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let index = row[1].as_i128().expect("a member index") as usize;
        Ok(match row[0].data() {
            ValueData::Struct(fields) if fields[0].as_i128() == Some(index as i128) => fields[index + 1].clone(),
            _ => Value::null(ty.clone()),
        })
    })
}

/// `list_extract`: 1-based, negative from the end, NULL out of range.
fn list_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        let (ValueData::List(items), Some(index)) = (row[0].data(), row[1].as_i128()) else {
            return Ok(Value::null(ty.clone()));
        };
        let position = match index {
            0 => None,
            i if i > 0 => Some(i - 1),
            i => Some(i + items.len() as i128),
        };
        let item = position
            .and_then(|p| usize::try_from(p).ok())
            .and_then(|p| items.get(p));
        Ok(item.cloned().unwrap_or_else(|| Value::null(ty.clone())))
    })
}

/// `array_extract` on a VARCHAR: the character at a position.
fn string_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    row_wise(args, result, |row| {
        let (ValueData::Bytes(bytes), Some(index)) = (row[0].data(), row[1].as_i128()) else {
            return Ok(Value::null(LogicalType::VARCHAR));
        };
        let text = String::from_utf8_lossy(bytes);
        let offset = i64::try_from(index).unwrap_or(if index < 0 { i64::MIN } else { i64::MAX });
        super::strings::check_substring_range(bytes.len(), offset, 1)?;
        let length = text.chars().count() as i128;
        let position = if index < 0 { index + length } else { index - 1 };
        let character = usize::try_from(position)
            .ok()
            .filter(|_| index != 0)
            .and_then(|p| text.chars().nth(p));
        Ok(Value::varchar(character.map(String::from).unwrap_or_default()))
    })
}

/// `map_extract_value`: the value of a key, or NULL.
fn map_extract(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if let ValueData::List(entries) = row[0].data() {
            for entry in entries {
                if let ValueData::Struct(fields) = entry.data()
                    && fields[0] == row[1]
                {
                    return Ok(fields[1].clone());
                }
            }
        }
        Ok(Value::null(ty.clone()))
    })
}

pub(super) fn bound_function(
    name: &str,
    children: Vec<BoundExpr>,
    return_type: LogicalType,
    callback: super::ScalarCallback,
) -> BoundExpr {
    let arguments = children.iter().map(|c| c.return_type.clone()).collect();
    let function = ScalarFunction::new(name, arguments, return_type.clone(), callback);
    BoundExpr::new(
        BoundExprKind::Function {
            function: Arc::new(function),
            children,
            is_operator: false,
        },
        return_type,
    )
}

/// The type of the elements of `list_value` and `array_value`.
fn element_type(name: &str, children: &[BoundExpr]) -> Result<LogicalType> {
    let mut element = LogicalType::SQLNULL;
    for child in children {
        let ty = expression_return_type(child);
        element = try_max_logical_type(&element, &ty).ok_or_else(|| {
            let types: Vec<String> = children.iter().map(|c| c.return_type.to_string()).collect();
            binder_error(format!(
                "No function matches the given name and argument types '{name}({})'. You might need to add explicit \
                 type casts.\nCannot deduce template type 'T' in function: '{name}'\nCandidates for 'T': {element}, {ty}",
                types.join(", ")
            ))
        })?;
    }
    Ok(normalize_type(&element))
}

fn bind_list_value(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let element = element_type(name, &children)?;
    let children = children.into_iter().map(|c| c.cast_to(&element, false)).collect();
    Ok(bound_function(name, children, LogicalType::list(element), list_value))
}

/// `UnpivotBind`: a list of columns, whose types have to go together.
fn bind_unpivot_list(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let mut element = LogicalType::SQLNULL;
    for child in &children {
        let ty = expression_return_type(child);
        element = try_max_logical_type(&element, &ty).ok_or_else(|| {
            binder_error(format!(
                "Cannot unpivot columns of types {element} and {ty} - an explicit cast is required"
            ))
        })?;
    }
    let element = normalize_type(&element);
    let children = children.into_iter().map(|c| c.cast_to(&element, false)).collect();
    Ok(bound_function(
        "unpivot_list",
        children,
        LogicalType::list(element),
        list_value,
    ))
}

fn bind_array_value(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    if children.is_empty() {
        return Err(invalid_input("array_value requires at least one argument"));
    }
    let element = element_type("array_value", &children)?;
    let size = children.len() as u32;
    let children = children.into_iter().map(|c| c.cast_to(&element, false)).collect();
    Ok(bound_function(
        "array_value",
        children,
        LogicalType::array(element, size),
        list_value,
    ))
}

pub fn bind_struct_pack(names: Vec<String>, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let mut fields: Vec<(String, LogicalType)> = Vec::with_capacity(children.len());
    for (name, child) in names.into_iter().zip(&children) {
        let name = if name.is_empty() { child.alias.clone() } else { name };
        if name.is_empty() {
            return Err(binder_error("Need named argument for struct pack, e.g., a := b"));
        }
        if fields.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
            return Err(binder_error(format!("Duplicate struct entry name \"{name}\"")));
        }
        fields.push((name, child.return_type.clone()));
    }
    Ok(bound_function(
        "struct_pack",
        children,
        LogicalType::struct_type(fields),
        struct_pack,
    ))
}

fn bind_row(children: Vec<BoundExpr>) -> BoundExpr {
    let types = children.iter().map(|c| c.return_type.clone()).collect();
    bound_function("row", children, LogicalType::tuple(types), struct_pack)
}

fn bind_union_value(names: Vec<String>, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let ([name], [child]) = (names.as_slice(), children.as_slice()) else {
        return Err(binder_error("union_value takes exactly one argument"));
    };
    let name = if name.is_empty() {
        child.alias.clone()
    } else {
        name.clone()
    };
    if name.is_empty() {
        return Err(binder_error(
            "Need named argument for union tag, e.g. UNION_VALUE(a := b)",
        ));
    }
    let ty = LogicalType::union_type(vec![(name, child.return_type.clone())]);
    Ok(bound_function("union_value", children, ty, union_value))
}

/// `union_tag`: the name of the selected member, as an ENUM of the member names.
fn union_tag(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        Ok(match row[0].data() {
            ValueData::Struct(fields) => match fields[0].as_i128() {
                Some(tag) => Value::enum_value(&ty, tag as usize),
                None => Value::null(ty.clone()),
            },
            _ => Value::null(ty.clone()),
        })
    })
}

fn bind_union_tag(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let ty = children[0].return_type.clone();
    if ty.id() == LogicalTypeId::SqlNull {
        return Ok(BoundExpr::constant(Value::sql_null()));
    }
    if ty.id() != LogicalTypeId::Union {
        return Err(binder_error(
            "First argument to union_tag function must be a union type.",
        ));
    }
    let names = ty.struct_children()[1..].iter().map(|(n, _)| n.clone()).collect();
    Ok(bound_function(
        "union_tag",
        children,
        LogicalType::enum_type(names),
        union_tag,
    ))
}

/// `enum_code`: the position of a value in its ENUM.
fn enum_code(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = result.logical_type().clone();
    row_wise(args, result, |row| {
        if row[0].is_null() {
            return Ok(Value::null(ty.clone()));
        }
        row[0].reinterpret(&ty)
    })
}

/// `enum_first`, `enum_last`, `enum_range` and `enum_code`.
fn bind_enum_function(name: &str, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let ty = children[0].return_type.clone();
    if ty.id() != LogicalTypeId::Enum {
        return Err(binder_error("This function needs an ENUM as an argument"));
    }
    let values = ty.enum_values();
    let text = |value: Option<&String>| value.map_or(Value::null(LogicalType::VARCHAR), |v| Value::varchar(v.clone()));
    Ok(match name {
        "enum_first" => BoundExpr::constant(text(values.first())),
        "enum_last" => BoundExpr::constant(text(values.last())),
        "enum_range" => {
            let items = values.iter().map(|v| Value::varchar(v.clone())).collect();
            BoundExpr::constant(Value::list(LogicalType::VARCHAR, items)?)
        }
        _ => {
            let code = match ty.physical_type() {
                PhysicalType::UInt8 => LogicalType::UTINYINT,
                PhysicalType::UInt16 => LogicalType::USMALLINT,
                _ => LogicalType::UINTEGER,
            };
            bound_function("enum_code", children, code, enum_code)
        }
    })
}

/// `enum_range_boundary`: the values between two values of an ENUM; NULL leaves that side open.
fn enum_range_boundary(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let ty = args.column(0).logical_type().clone();
    let enum_type = if ty.id() == LogicalTypeId::Enum {
        ty
    } else {
        args.column(1).logical_type().clone()
    };
    let values = enum_type.enum_values();
    let position = |value: &Value| {
        value
            .reinterpret(&LogicalType::new(value_type(&enum_type)))
            .ok()?
            .as_i128()
    };
    row_wise(args, result, |row| {
        let start = position(&row[0]).unwrap_or(0) as usize;
        let end = position(&row[1]).map_or(values.len(), |e| e as usize + 1);
        let items = values
            .iter()
            .take(end)
            .skip(start)
            .map(|v| Value::varchar(v.clone()))
            .collect();
        Value::list(LogicalType::VARCHAR, items)
    })
}

/// The unsigned integer type an ENUM is stored as.
fn value_type(ty: &LogicalType) -> LogicalTypeId {
    match ty.physical_type() {
        PhysicalType::UInt8 => LogicalTypeId::UTinyInt,
        PhysicalType::UInt16 => LogicalTypeId::USmallInt,
        _ => LogicalTypeId::UInteger,
    }
}

fn bind_enum_range_boundary(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let [start, end] = children.as_slice() else {
        return Err(binder_error("This function needs an ENUM as an argument"));
    };
    let is_enum = |c: &BoundExpr| c.return_type.id() == LogicalTypeId::Enum;
    let is_null = |c: &BoundExpr| c.return_type.id() == LogicalTypeId::SqlNull;
    if (!is_enum(start) && !is_null(start)) || (!is_enum(end) && !is_null(end)) || (is_null(start) && is_null(end)) {
        return Err(binder_error("This function needs an ENUM as an argument"));
    }
    if is_enum(start) && is_enum(end) && start.return_type != end.return_type {
        return Err(binder_error("The parameters need to link to ONLY one enum OR be NULL "));
    }
    Ok(bound_function(
        "enum_range_boundary",
        children,
        LogicalType::list(LogicalType::VARCHAR),
        enum_range_boundary,
    ))
}

fn bind_map(children: Vec<BoundExpr>) -> Result<BoundExpr> {
    match <[BoundExpr; 2]>::try_from(children) {
        Ok([keys, values]) => {
            let element = |child: &BoundExpr, what: &str| match child.return_type.id() {
                LogicalTypeId::List | LogicalTypeId::Array => Ok(child.return_type.child_type().unwrap().clone()),
                LogicalTypeId::SqlNull => Ok(LogicalType::SQLNULL),
                _ => Err(binder_error(format!(
                    "MAP {what} must be a LIST, not {}",
                    child.return_type
                ))),
            };
            let (key, value) = (element(&keys, "keys")?, element(&values, "values")?);
            let keys = keys.cast_to(&LogicalType::list(key.clone()), false);
            let values = values.cast_to(&LogicalType::list(value.clone()), false);
            Ok(bound_function(
                "map",
                vec![keys, values],
                LogicalType::map(key, value),
                map_function,
            ))
        }
        Err(children) if children.is_empty() => Ok(bound_function(
            "map",
            children,
            LogicalType::map(LogicalType::SQLNULL, LogicalType::SQLNULL),
            map_function,
        )),
        Err(children) => {
            let types: Vec<String> = children.iter().map(|c| c.return_type.to_string()).collect();
            Err(binder_error(format!(
                "No function matches the given name and argument types 'map({})'. You might need to add explicit \
                 type casts.",
                types.join(", ")
            )))
        }
    }
}

/// `ResolveStructEntryByName`.
fn struct_field_by_name(ty: &LogicalType, key: &str) -> Result<usize> {
    if key.is_empty() {
        return Err(binder_error("Key name for struct_extract must not be empty"));
    }
    let children = ty.struct_children();
    if let Some(index) = children.iter().position(|(n, _)| n.eq_ignore_ascii_case(key)) {
        return Ok(index);
    }
    let scored: Vec<(String, f64)> = children
        .iter()
        .map(|(n, _)| (n.clone(), similarity_rating(n, key)))
        .collect();
    let message = candidates_message(&top_n_strings(scored), "Candidate Entries");
    Err(binder_error(format!("Could not find key {key} in struct\n{message}")))
}

/// `ResolveStructEntryByIndex`: a 1-based index, as a 0-based one.
fn struct_field_by_index(ty: &LogicalType, index: i64) -> Result<usize> {
    let size = ty.struct_children().len();
    if index <= 0 || index as usize > size {
        return Err(binder_error(format!(
            "Key index {index} for struct_extract out of range - expected an index between 1 and {size}"
        )));
    }
    Ok(index as usize - 1)
}

/// `union_extract` with a constant key.
fn bind_union_extract(child: BoundExpr, key: &BoundExpr) -> Result<BoundExpr> {
    let ty = child.return_type.clone();
    let BoundExprKind::Constant(key) = &key.kind else {
        return Err(binder_error("Key name for union_extract needs to be a constant string"));
    };
    let key = key.try_to_string()?;
    let members = &ty.struct_children()[1..];
    let Some(index) = members.iter().position(|(n, _)| n.eq_ignore_ascii_case(&key)) else {
        let scored: Vec<(String, f64)> = members
            .iter()
            .map(|(n, _)| (n.clone(), similarity_rating(n, &key)))
            .collect();
        let message = candidates_message(&top_n_strings(scored), "Candidate Entries");
        return Err(binder_error(format!(
            "Could not find key \"{key}\" in union\n{message}"
        )));
    };
    let member = members[index].1.clone();
    let index = BoundExpr::constant(Value::bigint(index as i64));
    Ok(bound_function(
        "union_extract",
        vec![child, index],
        member,
        union_extract,
    ))
}

/// `StructExtractResolveTypes` with a constant key.
fn bind_struct_extract(child: BoundExpr, key: BoundExpr) -> Result<BoundExpr> {
    let ty = child.return_type.clone();
    if ty.id() == LogicalTypeId::Union {
        return bind_union_extract(child, &key);
    }
    if ty.id() == LogicalTypeId::Variant {
        return super::variant::bind_extract(child, key);
    }
    if super::json::is_json_type(&ty) {
        return super::json::bind_function("json_extract", vec![child, key], &|_| Ok(LogicalType::VARCHAR));
    }
    if !matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Tuple) {
        return Err(binder_error(format!(
            "struct_extract can only be used on a STRUCT, not on {ty}"
        )));
    }
    if ty.struct_children().is_empty() {
        return Err(binder_error("Can't extract something from an empty struct"));
    }
    let BoundExprKind::Constant(key) = &key.kind else {
        return Err(binder_error(
            "Argument #2 in function struct_extract must be a constant expression",
        ));
    };
    let index = if key.logical_type().id() == LogicalTypeId::Varchar {
        if ty.id() == LogicalTypeId::Tuple {
            return Err(binder_error(
                "struct_extract with a string key cannot be used on an unnamed struct, use a numeric index instead",
            ));
        }
        struct_field_by_name(&ty, &key.try_to_string()?)?
    } else {
        if ty.id() != LogicalTypeId::Tuple {
            return Err(binder_error(
                "struct_extract with an integer key can only be used on unnamed structs, use a string key instead",
            ));
        }
        struct_field_by_index(&ty, key.as_i128().unwrap_or(0) as i64)?
    };
    let member = ty.struct_children()[index].1.clone();
    let index = BoundExpr::constant(Value::bigint(index as i64));
    Ok(bound_function(
        "struct_extract",
        vec![child, index],
        member,
        struct_extract,
    ))
}

/// `list_extract` and `array_extract`, on anything that can be indexed.
fn bind_extract(name: &str, child: BoundExpr, key: BoundExpr) -> Result<BoundExpr> {
    let ty = child.return_type.clone();
    match ty.id() {
        LogicalTypeId::Struct | LogicalTypeId::Tuple | LogicalTypeId::Union | LogicalTypeId::Variant => {
            bind_struct_extract(child, key)
        }
        LogicalTypeId::SqlNull => Ok(BoundExpr::constant(Value::sql_null())),
        LogicalTypeId::List | LogicalTypeId::Array => {
            let element = ty.child_type().expect("a child type").clone();
            let key = key.cast_to(&LogicalType::BIGINT, false);
            Ok(bound_function(name, vec![child, key], element, list_extract))
        }
        LogicalTypeId::Map => {
            let children = ty.child_type().expect("an entry type").struct_children();
            let (key_type, value_type) = (children[0].1.clone(), children[1].1.clone());
            let key = key.cast_to(&key_type, false);
            Ok(bound_function(name, vec![child, key], value_type, map_extract))
        }
        _ if ty.physical_type() == PhysicalType::Varchar => {
            let key = key.cast_to(&LogicalType::BIGINT, false);
            Ok(bound_function(
                name,
                vec![child, key],
                LogicalType::VARCHAR,
                string_extract,
            ))
        }
        _ => Err(binder_error(format!(
            "No function matches the given name and argument types '{name}({ty}, {})'. You might need to add \
             explicit type casts.",
            key.return_type
        ))),
    }
}

/// Binds a function of [`is_nested_function`]; `names` are the names of its arguments.
pub fn bind_nested_function(name: &str, names: Vec<String>, children: Vec<BoundExpr>) -> Result<BoundExpr> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "list_value" | "list_pack" => bind_list_value(&lower, children),
        "unpivot_list" => bind_unpivot_list(children),
        "array_value" => bind_array_value(children),
        "row" => Ok(bind_row(children)),
        "struct_pack" => bind_struct_pack(names, children),
        "map" => bind_map(children),
        "union_value" => bind_union_value(names, children),
        "union_tag" | "enum_first" | "enum_last" | "enum_range" | "enum_code" if children.len() != 1 => {
            Err(binder_error(format!(
                "No function matches the given name and argument types of '{lower}'. You might need to add explicit \
                 type casts."
            )))
        }
        "union_tag" => bind_union_tag(children),
        "enum_first" | "enum_last" | "enum_range" | "enum_code" => bind_enum_function(&lower, children),
        "enum_range_boundary" => bind_enum_range_boundary(children),
        _ => {
            let Ok([child, key]) = <[BoundExpr; 2]>::try_from(children) else {
                return Err(binder_error(format!(
                    "No function matches the given name and argument types of '{lower}'. You might need to add \
                     explicit type casts."
                )));
            };
            match lower.as_str() {
                "struct_extract" | "struct_extract_at" | "union_extract" => bind_struct_extract(child, key),
                _ => bind_extract(&lower, child, key),
            }
        }
    }
}
