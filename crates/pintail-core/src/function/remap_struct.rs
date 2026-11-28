// Ported from duckdb/src/function/scalar/struct/remap_struct.cpp @ rust-port-baseline
//
//! `remap_struct(input, target_type, mapping, defaults)`: the fields of a nested value moved, renamed
//! and filled in to make a value of another type. C++ derives the remapping when the function is
//! bound and keeps it as bind data; a callback here has none, so the binder checks the arguments as
//! `VerifyRemapArgumentTypes` and `GetRemapEntries` do, and the callback derives the same remapping
//! from the types of its arguments and the constant mapping it is given.

use super::nested::{bound_function, row_wise};
use crate::execution::expression_executor::evaluate_scalar;
use crate::execution::parquet::cast_value;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::{Value, ValueData};
use pintail_common::vector::{DataChunk, Vector};
use pintail_common::{Error, ErrorKind, Result};

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message.into())
}

/// `IsRemappable`
fn remappable(ty: &LogicalType) -> bool {
    ty.is_nested() && ty.id() != LogicalTypeId::Variant
}

fn is_struct(ty: &LogicalType) -> bool {
    matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Tuple)
}

fn is_unnamed(ty: &LogicalType) -> bool {
    ty.id() == LogicalTypeId::Tuple || ty.struct_children().iter().any(|(name, _)| name.is_empty())
}

/// `RemapColumnInfo`: where a field of the target comes from.
#[derive(Debug, Clone, Default, PartialEq)]
struct ColumnInfo {
    index: Option<usize>,
    default_index: Option<usize>,
    children: Vec<ColumnInfo>,
}

/// `RemapIndex::GetMap`: the members of a nested type, by name.
fn members(ty: &LogicalType) -> Result<Vec<(String, LogicalType)>> {
    match ty.id() {
        LogicalTypeId::Struct | LogicalTypeId::Tuple => Ok(ty.struct_children().to_vec()),
        LogicalTypeId::List => Ok(vec![(
            "list".into(),
            ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL),
        )]),
        LogicalTypeId::Map => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            Ok(entry.struct_children().to_vec())
        }
        _ => Err(binder_error(format!("Can't remap type {ty}"))),
    }
}

fn find<'a, T>(entries: &'a [(String, T)], name: &str) -> Option<(usize, &'a T)> {
    entries
        .iter()
        .enumerate()
        .find(|(_, (n, _))| n.eq_ignore_ascii_case(name))
        .map(|(at, (_, t))| (at, t))
}

/// `RemapEntry`
#[derive(Debug, Clone)]
struct Entry {
    index: Option<usize>,
    default_index: Option<usize>,
    target_type: LogicalType,
    children: Option<Vec<(String, Entry)>>,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            index: None,
            default_index: None,
            target_type: LogicalType::SQLNULL,
            children: None,
        }
    }
}

fn entry_mut<'a>(entries: &'a mut [(String, Entry)], name: &str) -> Option<&'a mut Entry> {
    entries
        .iter_mut()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, e)| e)
}

/// `RemapEntry::PerformRemap`
fn perform_remap(
    target: &str,
    remap: &Value,
    source: &[(String, LogicalType)],
    targets: &[(String, LogicalType)],
    result: &mut Vec<(String, Entry)>,
    parent: &LogicalType,
) -> Result<()> {
    let ty = remap.logical_type();
    let (source_name, nested) = if ty.id() == LogicalTypeId::Varchar {
        (remap.try_to_string()?, None)
    } else if is_struct(ty) {
        // the remap spec is a (source_name, nested_spec) pair, built as an unnamed TUPLE
        if ty.id() != LogicalTypeId::Tuple {
            return Err(binder_error(
                "Remap keys for remap_struct needs to be an unnamed struct",
            ));
        }
        let ValueData::Struct(children) = remap.data() else {
            return Err(binder_error("Remap keys for remap_struct needs to have two children"));
        };
        if children.len() != 2 {
            return Err(binder_error("Remap keys for remap_struct needs to have two children"));
        }
        if children[0].logical_type().id() != LogicalTypeId::Varchar || !is_struct(children[1].logical_type()) {
            return Err(binder_error(
                "Remap keys for remap_struct need to be varchar and struct",
            ));
        }
        (children[0].try_to_string()?, Some(children[1].clone()))
    } else {
        return Err(binder_error(
            "Remap keys for remap_struct needs to be a string or struct",
        ));
    };
    let mut found = find(source, &source_name);
    if found.is_none() && parent.id() == LogicalTypeId::List {
        // a LIST has one child, whatever name its producer gave it
        found = find(source, "list");
    }
    let Some((source_index, source_type)) = found else {
        return Err(binder_error(format!("Source value {source_name} not found")));
    };
    let Some((_, target_type)) = find(targets, target) else {
        return Err(binder_error(format!("Target value {target} not found")));
    };
    let nested = nested.filter(|n| !n.is_null());
    let mut entry = Entry {
        index: Some(source_index),
        target_type: target_type.clone(),
        ..Entry::default()
    };
    if source_type.id() == LogicalTypeId::SqlNull && nested.is_none() {
        result.push((target.to_string(), entry));
        return Ok(());
    }
    let (source_nested, target_nested) = (remappable(source_type), remappable(target_type));
    if source_nested || target_nested || nested.is_some() {
        if source_type.id() != target_type.id() {
            return Err(binder_error(format!(
                "Can't change source type ({source_type}) to target type ({target_type}), type conversion not allowed"
            )));
        }
        if let Some(nested) = nested {
            if !source_nested || !target_nested {
                return Err(binder_error(format!(
                    "Found a struct value ({}) as a remap, this is only expected for a nested type, source \
                     type is '{source_type}', target type is '{target_type}'",
                    nested.try_to_string()?
                )));
            }
            let mut children = Vec::new();
            let (child_source, child_targets) = (members(source_type)?, members(target_type)?);
            let ValueData::Struct(values) = nested.data() else {
                unreachable!("a struct value")
            };
            for ((name, _), value) in nested.logical_type().struct_children().iter().zip(values) {
                perform_remap(name, value, &child_source, &child_targets, &mut children, source_type)?;
            }
            entry.children = Some(children);
        }
    }
    result.push((target.to_string(), entry));
    Ok(())
}

/// `RemapEntry::HandleDefault`
fn handle_default(
    default_index: usize,
    target: &str,
    default_type: &LogicalType,
    targets: &[(String, LogicalType)],
    result: &mut Vec<(String, Entry)>,
) -> Result<()> {
    let Some((_, target_type)) = find(targets, target) else {
        return Err(binder_error(format!("Default value {target} not found for remap")));
    };
    if default_type.id() == LogicalTypeId::Struct {
        if !remappable(target_type) {
            return Err(binder_error(format!(
                "Default value is a struct - target value should be a nested type, is '{target_type}'"
            )));
        }
        if entry_mut(result, target).is_none() {
            result.push((
                target.to_string(),
                Entry {
                    default_index: Some(default_index),
                    children: Some(Vec::new()),
                    ..Entry::default()
                },
            ));
        }
        let entry = entry_mut(result, target).expect("added above");
        entry.default_index = Some(default_index);
        let child_targets = members(target_type)?;
        let Some(children) = entry.children.as_mut() else {
            return Err(binder_error("No child remaps found"));
        };
        for (at, (name, ty)) in default_type.struct_children().iter().enumerate() {
            handle_default(at, name, ty, &child_targets, children)?;
        }
        return Ok(());
    }
    if strip_literal(default_type) != *target_type {
        return Err(binder_error(format!(
            "Default key {target} does not match target type {target_type} - add a cast"
        )));
    }
    if entry_mut(result, target).is_some() {
        return Err(binder_error(format!("Duplicate value provided for target {target}")));
    }
    result.push((
        target.to_string(),
        Entry {
            default_index: Some(default_index),
            ..Entry::default()
        },
    ));
    Ok(())
}

/// The type a literal has once it is a value of a column: `'hello'` is a VARCHAR.
fn strip_literal(ty: &LogicalType) -> LogicalType {
    match ty.id() {
        LogicalTypeId::StringLiteral => LogicalType::VARCHAR,
        LogicalTypeId::IntegerLiteral => ty.integer_literal_value().map_or(LogicalType::INTEGER, |(ty, _, _)| ty),
        _ => ty.clone(),
    }
}

/// `RemapEntry::ConstructMap`
fn construct_map(ty: &LogicalType, entries: &[(String, Entry)]) -> Result<Vec<ColumnInfo>> {
    let mut result = Vec::new();
    for (name, child) in members(ty)? {
        let Some((_, entry)) = find(entries, &name) else {
            return Err(binder_error(format!("Missing target value \"{name}\" for remap")));
        };
        let mut info = ColumnInfo {
            index: entry.index,
            default_index: entry.default_index,
            children: Vec::new(),
        };
        if remappable(&child)
            && let Some(children) = &entry.children
        {
            info.children = construct_map(&child, children)?;
        }
        result.push(info);
    }
    Ok(result)
}

/// The check of `RemapEntry::RemapCast`: one source field cannot become targets of different types.
fn check_cast(entries: &[(String, Entry)]) -> Result<()> {
    let mut sources: Vec<(usize, &Entry)> = Vec::new();
    for (_, entry) in entries {
        let Some(index) = entry.index else { continue };
        match sources.iter().find(|(i, _)| *i == index) {
            Some((_, other)) if other.target_type != entry.target_type => {
                return Err(binder_error(format!(
                    "remap_struct cannot map a single source field to targets of different types ('{}' and '{}')",
                    other.target_type, entry.target_type
                )));
            }
            Some(_) => {}
            None => sources.push((index, entry)),
        }
        if let Some(children) = &entry.children {
            check_cast(children)?;
        }
    }
    Ok(())
}

/// `VerifyRemapArgumentTypes`
fn verify(types: &[LogicalType]) -> Result<()> {
    for (at, ty) in types.iter().take(3).enumerate() {
        if ty.id() == LogicalTypeId::SqlNull && at == 2 {
            continue;
        }
        if !remappable(ty) {
            return Err(binder_error(format!(
                "Struct remap can only remap nested types, not '{ty}'"
            )));
        }
        if is_struct(ty) && is_unnamed(ty) {
            return Err(binder_error("Struct remap can only remap named structs"));
        }
    }
    let (from, to, mapping, defaults) = (&types[0], &types[1], &types[2], &types[3]);
    if mapping.id() != LogicalTypeId::SqlNull && !is_struct(mapping) {
        return Err(binder_error(format!(
            "The mapping provided to 'remap_struct' should be of type STRUCT if it's not NULL, not '{mapping}'"
        )));
    }
    if defaults.id() != LogicalTypeId::SqlNull && !is_struct(defaults) {
        return Err(binder_error(
            "The defaults provided to 'remap_struct' should be of type STRUCT if they're not NULL",
        ));
    }
    if is_struct(defaults) && is_unnamed(defaults) {
        return Err(binder_error(
            "The defaults have to be either NULL or a named STRUCT, not an unnamed struct",
        ));
    }
    if (remappable(from) || remappable(to)) && from.id() != to.id() {
        return Err(binder_error(format!(
            "Can't change source type ({from}) to target type ({to}), type conversion not allowed"
        )));
    }
    Ok(())
}

/// `GetRemapEntries` + `ConstructMap`: the remapping, from the types and the mapping.
fn remapping(types: &[LogicalType], mapping: &Value) -> Result<Vec<ColumnInfo>> {
    let (source, targets) = (members(&types[0])?, members(&types[1])?);
    let mut entries = Vec::new();
    if !mapping.is_null()
        && let ValueData::Struct(values) = mapping.data()
    {
        for ((target, _), value) in types[2].struct_children().iter().zip(values) {
            perform_remap(target, value, &source, &targets, &mut entries, &types[0])?;
        }
    }
    if types[3].id() != LogicalTypeId::SqlNull {
        for (at, (target, ty)) in types[3].struct_children().iter().enumerate() {
            handle_default(at, target, ty, &targets, &mut entries)?;
        }
    }
    check_cast(&entries)?;
    construct_map(&types[1], &entries)
}

/// Binds `remap_struct`.
pub(super) fn bind(mut children: Vec<BoundExpr>) -> Result<BoundExpr> {
    if children.len() != 4 {
        return Err(binder_error(format!(
            "remap_struct expects 4 arguments, got {}",
            children.len()
        )));
    }
    let types: Vec<LogicalType> = children.iter().map(|c| c.return_type.clone()).collect();
    verify(&types)?;
    if !children[2].is_foldable() {
        return Err(binder_error("The mapping of remap_struct must be a constant"));
    }
    let mapping = evaluate_scalar(&children[2])?;
    if !children[3].is_foldable() {
        return Err(binder_error("Default values must be constants"));
    }
    remapping(&types, &mapping)?;
    // the mapping and the defaults are read by the callback as they are
    children[2] = BoundExpr::constant(mapping);
    let defaults = evaluate_scalar(&children[3])?;
    children[3] = BoundExpr::constant(defaults);
    if !matches!(children[1].kind, BoundExprKind::Constant(_)) {
        children[1] = BoundExpr::constant(Value::null(types[1].clone()));
    }
    Ok(bound_function("remap_struct", children, types[1].clone(), remap_struct))
}

fn remap_struct(args: &DataChunk, result: &mut Vector) -> Result<()> {
    let types: Vec<LogicalType> = args.columns().iter().map(|c| c.logical_type().clone()).collect();
    let mapping = args.column(2).get_value(0);
    let info = remapping(&types, &mapping)?;
    let target = types[1].clone();
    row_wise(args, result, |row| remap(&row[0], &row[3], &target, &info))
}

fn fields(value: &Value) -> &[Value] {
    match value.data() {
        ValueData::Struct(fields) => fields,
        _ => &[],
    }
}

/// `RemapNested`, of one value.
fn remap(input: &Value, defaults: &Value, target: &LogicalType, info: &[ColumnInfo]) -> Result<Value> {
    if input.is_null() {
        return Ok(Value::null(target.clone()));
    }
    let defaults_fields = fields(defaults);
    match target.id() {
        LogicalTypeId::Struct => {
            let inputs = fields(input);
            let mut result = Vec::with_capacity(info.len());
            for ((_, ty), column) in target.struct_children().iter().zip(info) {
                result.push(remap_child(column, inputs, defaults, defaults_fields, ty)?);
            }
            Ok(Value::new(target.clone(), ValueData::Struct(result))?)
        }
        LogicalTypeId::List => {
            let child = target.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let ValueData::List(items) = input.data() else {
                return Ok(Value::null(target.clone()));
            };
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                result.push(remap_child(
                    &info[0],
                    std::slice::from_ref(item),
                    defaults,
                    defaults_fields,
                    &child,
                )?);
            }
            Ok(Value::new(target.clone(), ValueData::List(result))?)
        }
        LogicalTypeId::Map => {
            let entry = target.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let kinds = entry.struct_children().to_vec();
            let ValueData::List(items) = input.data() else {
                return Ok(Value::null(target.clone()));
            };
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                let inputs = fields(item);
                let key = remap_child(&info[0], inputs, defaults, defaults_fields, &kinds[0].1)?;
                let value = remap_child(&info[1], inputs, defaults, defaults_fields, &kinds[1].1)?;
                result.push(Value::new(entry.clone(), ValueData::Struct(vec![key, value]))?);
            }
            Ok(Value::new(target.clone(), ValueData::List(result))?)
        }
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("Can't RemapNested for type '{target}'"),
        )),
    }
}

/// `RemapChildVectors`, of one field.
fn remap_child(
    column: &ColumnInfo,
    inputs: &[Value],
    defaults: &Value,
    defaults_fields: &[Value],
    ty: &LogicalType,
) -> Result<Value> {
    let default = column.default_index.and_then(|at| defaults_fields.get(at));
    if let Some(index) = column.index
        && !column.children.is_empty()
    {
        return remap(&inputs[index], default.unwrap_or(defaults), ty, &column.children);
    }
    if column.index.is_none()
        && !column.children.is_empty()
        && let Some(default) = default
        && default.logical_type().id() == LogicalTypeId::Struct
        && ty.id() == LogicalTypeId::Struct
    {
        // a nested default without a source: its fields remapped to the target type
        return remap(default, default, ty, &column.children);
    }
    let value = match (default, column.index) {
        (Some(default), _) => default,
        (None, Some(index)) => &inputs[index],
        (None, None) => return Ok(Value::null(ty.clone())),
    };
    if value.logical_type() == ty {
        Ok(value.clone())
    } else {
        cast_value(value, ty)
    }
}
