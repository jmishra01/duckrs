// Ported from duckdb/src/function/cast/{list_casts,array_casts,struct_cast,tuple_cast,map_cast,
// union_casts,string_cast}.cpp @ rust-port-baseline
//
//! Casts of the nested types. Unlike DuckDB's they work value by value, recursing into the
//! children: a row of a TRY_CAST loses only what failed, as in DuckDB.

use super::cast::{CastMode, cast_vector, conversion_error};
use super::cast_rules::implicit_cast;
use super::string_nested::{split_list, split_map, split_struct};
use super::variant::{from_variant, to_variant};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::{Value, ValueData};
use duckrs_common::vector::{Vector, VectorType};
use duckrs_common::{Error, ErrorKind, Result};

fn unimplemented(source: &LogicalType, target: &LogicalType) -> Error {
    conversion_error(format!("Unimplemented type for cast ({source} -> {target})"))
}

fn is_unnamed(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Tuple || ty.struct_children().iter().all(|(n, _)| n.is_empty())
}

/// `StructBoundCastData::GetFieldMapping`: for each target field, the source field it takes.
fn field_mapping(source: &LogicalType, target: &LogicalType) -> Result<Vec<Option<usize>>> {
    let (from, to) = (source.struct_children(), target.struct_children());
    if is_unnamed(source) || is_unnamed(target) {
        if from.len() != to.len() {
            return Err(Error::new(
                ErrorKind::MismatchType,
                format!("Type {source} does not match with {target}. Cannot cast STRUCTs of different size"),
            ));
        }
        return Ok((0..to.len()).map(Some).collect());
    }
    let mut mapping: Vec<Option<usize>> = vec![None; to.len()];
    for (i, (name, _)) in to.iter().enumerate() {
        if to[..i].iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            return Err(Error::not_implemented(format!(
                "Error while casting - duplicate name {name} in struct"
            )));
        }
    }
    for (i, (name, _)) in from.iter().enumerate() {
        if let Some(index) = to.iter().position(|(n, _)| n.eq_ignore_ascii_case(name))
            && mapping[index].is_none()
        {
            mapping[index] = Some(i);
        }
    }
    if mapping.iter().all(Option::is_none) {
        return Err(Error::new(
            ErrorKind::Binder,
            format!(
                "STRUCT to STRUCT cast must have at least one matching member, inputs are ({source}) and ({target})"
            ),
        ));
    }
    Ok(mapping)
}

/// One value through the vector casts.
fn cast_scalar(value: &Value, target: &LogicalType, mode: CastMode) -> Result<Value> {
    let source = Vector::constant(value);
    let mut result = Vector::new(target.clone(), 1);
    cast_vector(&source, &mut result, 1, mode)?;
    Ok(result.get_value(0))
}

fn varchar_part(part: Option<Vec<u8>>) -> Value {
    match part {
        Some(bytes) => Value::new(LogicalType::VARCHAR, ValueData::Bytes(bytes)).expect("a VARCHAR value"),
        None => Value::null(LogicalType::VARCHAR),
    }
}

fn string_cast_error(input: &[u8], target: &LogicalType) -> Error {
    conversion_error(format!(
        "Type VARCHAR with value '{}' can't be cast to the destination type {target}",
        String::from_utf8_lossy(input)
    ))
}

/// `StringToNestedTypeCast`.
fn string_to_nested(input: &[u8], target: &LogicalType, mode: CastMode) -> Result<Value> {
    let error = || string_cast_error(input, target);
    match target.id() {
        LogicalTypeId::List | LogicalTypeId::Array => {
            let child = target.child_type().expect("a child type");
            let parts = split_list(input).ok_or_else(error)?;
            let items = parts
                .into_iter()
                .map(|p| cast_value(&varchar_part(p), child, mode))
                .collect::<Result<Vec<_>>>()?;
            if let Some(size) = target.array_size().filter(|_| target.id() == LogicalTypeId::Array)
                && items.len() != size as usize
            {
                return Err(conversion_error(format!(
                    "Type VARCHAR with value '{}' can't be cast to the destination type {target}, the size of the \
                     array must match the destination type",
                    String::from_utf8_lossy(input)
                )));
            }
            Value::new(target.clone(), ValueData::List(items))
        }
        LogicalTypeId::Map => {
            let entry = target.child_type().expect("an entry type");
            let fields = entry.struct_children();
            let entries = split_map(input).ok_or_else(error)?;
            let mut items = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let key = cast_value(&varchar_part(Some(key)), &fields[0].1, mode)?;
                if key.is_null() {
                    return Err(error());
                }
                let value = cast_value(&varchar_part(value), &fields[1].1, mode)?;
                items.push(Value::new(entry.clone(), ValueData::Struct(vec![key, value]))?);
            }
            check_unique_keys(&items)?;
            Value::new(target.clone(), ValueData::List(items))
        }
        _ => {
            let names: Vec<String> = target.struct_children().iter().map(|(n, _)| n.clone()).collect();
            let parts = split_struct(input, &names).ok_or_else(error)?;
            let fields = parts
                .into_iter()
                .zip(target.struct_children())
                .map(|(p, (_, ty))| cast_value(&varchar_part(p), ty, mode))
                .collect::<Result<Vec<_>>>()?;
            Value::new(target.clone(), ValueData::Struct(fields))
        }
    }
}

fn check_unique_keys(items: &[Value]) -> Result<()> {
    for (i, item) in items.iter().enumerate() {
        if items[..i].iter().any(|other| map_key(other) == map_key(item)) {
            return Err(Error::new(ErrorKind::InvalidInput, "Map keys must be unique."));
        }
    }
    Ok(())
}

fn map_key(entry: &Value) -> &Value {
    match entry.data() {
        ValueData::Struct(fields) => &fields[0],
        _ => unreachable!("a map entry is a STRUCT"),
    }
}

/// The items of a LIST or ARRAY as `target`'s children.
fn cast_items(items: &[Value], target: &LogicalType, mode: CastMode) -> Result<Vec<Value>> {
    let child = target.child_type().expect("a child type");
    items.iter().map(|v| cast_value(v, child, mode)).collect()
}

/// `StructToMapCast`: the names of a STRUCT as keys.
fn struct_to_map(value: &Value, fields: &[Value], target: &LogicalType, mode: CastMode) -> Result<Value> {
    let entry = target.child_type().expect("an entry type");
    let entry_fields = entry.struct_children();
    let mut items = Vec::with_capacity(fields.len());
    for ((name, _), field) in value.logical_type().struct_children().iter().zip(fields) {
        let key = cast_value(&Value::varchar(name.clone()), &entry_fields[0].1, mode)?;
        let member = cast_value(field, &entry_fields[1].1, mode)?;
        items.push(Value::new(entry.clone(), ValueData::Struct(vec![key, member]))?);
    }
    Value::new(target.clone(), ValueData::List(items))
}

/// The value of a UNION as `member` of `target`.
fn union_value(target: &LogicalType, member: usize, value: Value) -> Result<Value> {
    let mut fields: Vec<Value> = target.struct_children()[1..]
        .iter()
        .map(|(_, ty)| Value::null(ty.clone()))
        .collect();
    fields[member] = value;
    fields.insert(0, Value::utinyint(member as u8));
    Value::new(target.clone(), ValueData::Struct(fields))
}

/// `BindToUnionMemberCast`: the member of `target` a value of type `source` casts to.
fn union_member(source: &LogicalType, target: &LogicalType) -> Result<usize> {
    let members = &target.struct_children()[1..];
    let mut best = i64::MAX;
    let mut candidates: Vec<usize> = Vec::new();
    for (i, (_, member)) in members.iter().enumerate() {
        let cost = implicit_cast(source, member);
        if cost >= 0 && cost <= best {
            if cost != best {
                candidates.clear();
                best = cost;
            }
            candidates.push(i);
        }
    }
    match candidates.as_slice() {
        [member] => Ok(*member),
        [] => {
            let types: Vec<String> = members.iter().map(|(_, t)| t.to_string()).collect();
            Err(conversion_error(format!(
                "Type {source} can't be cast as {target}. {source} can't be implicitly cast to any of the union \
                 member types: {}",
                types.join(", ")
            )))
        }
        candidates => {
            let list: Vec<String> = candidates
                .iter()
                .map(|&i| format!("'{} ({})'", members[i].0, members[i].1))
                .collect();
            Err(conversion_error(format!(
                "Type {source} can't be cast as {target}. The cast is ambiguous, multiple possible members in \
                 target: {}. Disambiguate the target type by using the 'union_value(<tag> := <arg>)' function to \
                 promote the source value to a single member union before casting.",
                list.join(", ")
            )))
        }
    }
}

/// `StructToUnionCast::AllowImplicitCastFromStruct`: a STRUCT laid out like the UNION.
fn struct_matches_union(source: &LogicalType, target: &LogicalType) -> bool {
    let (from, to) = (source.struct_children(), target.struct_children());
    source.id() == LogicalTypeId::Struct
        && from.len() == to.len()
        && from[0].1 == to[0].1
        && from[1..]
            .iter()
            .zip(&to[1..])
            .all(|((from_name, from_type), (to_name, to_type))| {
                from_name.eq_ignore_ascii_case(to_name) && (from_type == to_type || *from_type == LogicalType::VARCHAR)
            })
}

/// `StructToUnionCast` + `UnionVector::CheckUnionValidity`.
fn struct_to_union(fields: &[Value], target: &LogicalType, mode: CastMode) -> Result<Value> {
    let error = |message: &str| conversion_error(message.to_string());
    let casted = fields
        .iter()
        .zip(target.struct_children())
        .map(|(field, (_, ty))| {
            let value = cast_value(field, ty, mode)?;
            // a VARCHAR is a JSON as it is: the value takes the member's type
            if value.logical_type() != ty {
                return Value::new(ty.clone(), value.data().clone());
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(tag) = casted[0].as_i128() else {
        return Err(error("One or more rows in the produced UNION have a NULL tag"));
    };
    let tag = tag as usize;
    let members = &casted[1..];
    if tag >= members.len() {
        return Err(error("One or more of the tags do not point to a valid union member"));
    }
    let valid: Vec<usize> = (0..members.len()).filter(|&i| !members[i].is_null()).collect();
    match valid.as_slice() {
        [] => {}
        [member] if *member == tag => {}
        [_] => {
            return Err(error(
                "One or more rows in the produced UNION have tags that don't point to the valid member",
            ));
        }
        _ => {
            return Err(error(
                "One or more rows in the produced UNION have validity set for more than 1 member",
            ));
        }
    }
    Value::new(target.clone(), ValueData::Struct(casted))
}

/// UNION to UNION, if every source member is a member of the target.
fn union_to_union(value: &Value, fields: &[Value], target: &LogicalType, mode: CastMode) -> Option<Result<Value>> {
    let source = value.logical_type();
    let (from, to) = (&source.struct_children()[1..], &target.struct_children()[1..]);
    let mut tags = Vec::with_capacity(from.len());
    for (name, ty) in from {
        let Some(index) = to.iter().position(|(n, _)| n.eq_ignore_ascii_case(name)) else {
            // the source may still cast to a member of the target as a whole
            if union_member(source, target).is_ok() {
                return None;
            }
            return Some(Err(conversion_error(format!(
                "Type {source} can't be cast as {target}. The member {name} is not present in target union"
            ))));
        };
        if !super::cast::has_cast(ty, &to[index].1) {
            return None;
        }
        tags.push(index);
    }
    let Some(tag) = fields[0].as_i128() else {
        return Some(Ok(Value::null(target.clone())));
    };
    let tag = tag as usize;
    // the member is cast strictly: a TRY_CAST that fails on it loses the whole UNION
    let member = cast_inner(&fields[tag + 1], &to[tags[tag]].1, CastMode::Strict);
    Some(match member {
        // a TRY_CAST that fails on the member loses the whole UNION
        Err(_) if mode == CastMode::Try => Ok(Value::null(target.clone())),
        member => member.and_then(|v| union_value(target, tags[tag], v)),
    })
}

/// Casts one value, which may be nested on either side. A TRY_CAST loses only what failed.
pub(super) fn cast_value(value: &Value, target: &LogicalType, mode: CastMode) -> Result<Value> {
    match cast_inner(value, target, mode) {
        Err(_) if mode == CastMode::Try => Ok(Value::null(target.clone())),
        result => result,
    }
}

fn cast_inner(value: &Value, target: &LogicalType, mode: CastMode) -> Result<Value> {
    use LogicalTypeId as L;
    if value.is_null() {
        // `ToUnionMemberCast` keeps the tag of a NULL of a member's type: the member is NULL
        let source = value.logical_type();
        if target.id() == L::Union
            && !matches!(source.id(), L::SqlNull | L::Union)
            && let Ok(member) = union_member(source, target)
        {
            let member_type = target.struct_children()[member + 1].1.clone();
            return union_value(target, member, Value::null(member_type));
        }
        return Ok(Value::null(target.clone()));
    }
    let source = value.logical_type();
    if source == target {
        return Ok(value.clone());
    }
    if target.id() == L::Variant {
        return to_variant(value);
    }
    if source.id() == L::Variant {
        return from_variant(value, target);
    }
    if target.id() == L::Union {
        if let (L::Union, ValueData::Struct(fields)) = (source.id(), value.data())
            && let Some(result) = union_to_union(value, fields, target, mode)
        {
            return result;
        }
        if let ValueData::Struct(fields) = value.data()
            && struct_matches_union(source, target)
        {
            return struct_to_union(fields, target, mode);
        }
        let member = union_member(source, target)?;
        let cast = cast_value(value, &target.struct_children()[member + 1].1, mode)?;
        return union_value(target, member, cast);
    }
    if target.id() == L::Varchar {
        return Ok(Value::varchar(value.try_to_string()?));
    }
    match (source.id(), value.data(), target.id()) {
        (L::Varchar, ValueData::Bytes(bytes), L::List | L::Array | L::Map | L::Struct | L::Tuple) => {
            string_to_nested(bytes, target, mode)
        }
        (L::List | L::Array, ValueData::List(items), L::List) => {
            Value::new(target.clone(), ValueData::List(cast_items(items, target, mode)?))
        }
        (L::List | L::Array, ValueData::List(items), L::Array) => {
            let size = target.array_size().unwrap_or(0) as usize;
            if items.len() != size {
                let message = if source.id() == L::List {
                    format!(
                        "Cannot cast list with length {} to array with length {size}",
                        items.len()
                    )
                } else {
                    format!("Cannot cast array of size {} to array of size {size}", items.len())
                };
                return Err(conversion_error(message));
            }
            Value::new(target.clone(), ValueData::List(cast_items(items, target, mode)?))
        }
        (L::Map, ValueData::List(items), L::Map) => {
            let items = cast_items(items, target, mode)?;
            if items.iter().any(|item| item.is_null() || map_key(item).is_null()) {
                return Err(Error::new(ErrorKind::InvalidInput, "Map keys can not be NULL."));
            }
            check_unique_keys(&items)?;
            Value::new(target.clone(), ValueData::List(items))
        }
        (L::Struct | L::Tuple, ValueData::Struct(fields), L::Struct | L::Tuple) => {
            let mapping = field_mapping(source, target)?;
            let casted = mapping
                .into_iter()
                .zip(target.struct_children())
                .map(|(from, (_, ty))| match from {
                    Some(from) => cast_value(&fields[from], ty, mode),
                    None => Ok(Value::null(ty.clone())),
                })
                .collect::<Result<Vec<_>>>()?;
            Value::new(target.clone(), ValueData::Struct(casted))
        }
        (L::Struct, ValueData::Struct(fields), L::Map) => struct_to_map(value, fields, target, mode),
        (L::Tuple, _, L::Map) => Err(conversion_error("Cannot cast unnamed STRUCTs to MAP".to_string())),
        (L::Enum, _, _) if target.is_nested() => cast_inner(&Value::varchar(value.try_to_string()?), target, mode),
        _ if source.is_nested() || target.is_nested() => Err(unimplemented(source, target)),
        _ => cast_scalar(value, target, mode),
    }
}

/// Whether every type inside of `ty` is `ported`.
fn all_leaves(ty: &LogicalType, ported: &dyn Fn(&LogicalType) -> bool) -> bool {
    match ty.id() {
        LogicalTypeId::List | LogicalTypeId::Array | LogicalTypeId::Map => {
            ty.child_type().is_some_and(|child| all_leaves(child, ported))
        }
        LogicalTypeId::Struct | LogicalTypeId::Tuple | LogicalTypeId::Union => {
            ty.has_params() && ty.struct_children().iter().all(|(_, child)| all_leaves(child, ported))
        }
        LogicalTypeId::SqlNull => true,
        _ => ported(ty),
    }
}

/// Whether [`nested_cast`] handles a cast of ported leaf types with a nested side.
pub(super) fn has_nested_cast(
    source: &LogicalType,
    target: &LogicalType,
    ported: &dyn Fn(&LogicalType) -> bool,
) -> bool {
    (source.is_nested() || target.is_nested()) && all_leaves(source, ported) && all_leaves(target, ported)
}

/// Casts row by row, through [`cast_value`].
pub(super) fn nested_cast(source: &Vector, result: &mut Vector, count: usize, mode: CastMode) -> Result<()> {
    let target = result.logical_type().clone();
    let constant = source.vector_type() == VectorType::Constant;
    let rows = if constant { 1 } else { count };
    result.prepare_flat(rows.max(1));
    for i in 0..rows {
        result.set_value(i, &cast_value(&source.get_value(i), &target, mode)?)?;
    }
    if constant {
        result.set_constant();
    }
    Ok(())
}
