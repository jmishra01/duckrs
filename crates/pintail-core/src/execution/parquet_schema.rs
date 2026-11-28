//! The `schema` option of `read_parquet` (`MultiFileReader::ParseSchemaOption` and the column mapping of
//! `MultiFileColumnMapper`): the columns of the result by field id or by name, with defaults for those a file
//! does not have.

use arrow::datatypes::{DataType, Field};
use pintail_common::types::{LogicalType, LogicalTypeId};
use pintail_common::value::{Value, ValueData};
use pintail_common::{Error, ErrorKind, Result};

/// How a column of `schema` finds the column of a file.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaKey {
    FieldId(i32),
    Name(String),
}

/// `MultiFileColumnDefinition`, as `schema` defines it.
#[derive(Debug, Clone)]
pub struct SchemaColumn {
    pub key: SchemaKey,
    pub name: String,
    pub ty: LogicalType,
    /// of `ty`
    pub default: Value,
    pub children: Vec<SchemaColumn>,
}

fn binder_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Binder, message.into())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message.into())
}

fn type_id_name(ty: &LogicalType) -> String {
    ty.id().name().to_string()
}

/// `VerifySchemaDefinitionType`.
fn verify_definition_type(ty: &LogicalType, root: bool) -> Result<()> {
    if ty.id() != LogicalTypeId::Struct {
        if root {
            return Err(invalid("'schema' expects a STRUCT as the value type of the map"));
        }
        return Err(binder_error(format!(
            "'schema' 'children' expects a STRUCT as the value type of the map, not {ty}"
        )));
    }
    let fields = ty.struct_children();
    if fields.len() != 3 && fields.len() != 4 {
        return Err(invalid(format!(
            "'schema' expects the STRUCT to have 3 or 4 fields, 'name', 'type', 'default_value' and optionally \
             'children', not {}",
            fields.len()
        )));
    }
    let expect = |index: usize, name: &str, ordinal: &str| -> Result<()> {
        if fields[index].0 != name {
            return Err(invalid(format!(
                "'schema' expects the {ordinal} field of the struct to be called '{name}'"
            )));
        }
        Ok(())
    };
    expect(0, "name", "first")?;
    if fields[0].1.id() != LogicalTypeId::Varchar {
        return Err(invalid(format!(
            "'schema' expects the 'name' field to be of type VARCHAR, not {}",
            type_id_name(&fields[0].1)
        )));
    }
    expect(1, "type", "second")?;
    if fields[1].1.id() != LogicalTypeId::Varchar {
        return Err(invalid(format!(
            "'schema' expects the 'type' field to be of type VARCHAR, not {}",
            type_id_name(&fields[1].1)
        )));
    }
    expect(2, "default_value", "third")?;
    if fields.len() == 4 {
        expect(3, "children", "fourth")?;
    }
    Ok(())
}

/// The named children a nested type has (`LogicalType::GetNamedChildTypes`).
fn named_children(ty: &LogicalType) -> Vec<(String, LogicalType)> {
    match ty.id() {
        LogicalTypeId::Struct => ty.struct_children().to_vec(),
        LogicalTypeId::List => vec![(
            "element".to_string(),
            ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL),
        )],
        LogicalTypeId::Map => ty
            .child_type()
            .map(|entry| entry.struct_children().to_vec())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// `VerifySchemaChildren`.
fn verify_children(column: &SchemaColumn) -> Result<()> {
    if !matches!(
        column.ty.id(),
        LogicalTypeId::Struct | LogicalTypeId::List | LogicalTypeId::Map
    ) {
        return Err(binder_error(format!(
            "'schema' column \"{}\" of type {} cannot define nested children",
            column.name, column.ty
        )));
    }
    let expected = named_children(&column.ty);
    if column.children.len() != expected.len() {
        return Err(binder_error(format!(
            "'schema' column \"{}\" of type {} expects {} child definitions, not {}",
            column.name,
            column.ty,
            expected.len(),
            column.children.len()
        )));
    }
    for (child, (name, ty)) in column.children.iter().zip(&expected) {
        let same_name = child.name == *name;
        let same_type = child.ty == *ty;
        if same_name && same_type {
            continue;
        }
        let mut error = String::new();
        if !same_name {
            error = format!("name \"{name}\" (got \"{}\")", child.name);
        }
        if !same_type {
            if error.is_empty() {
                error += &format!("name \"{name}\" to have ");
            } else {
                error += " and ";
            }
            error += &format!("type \"{ty}\" (got \"{}\")", child.ty);
        }
        return Err(binder_error(format!(
            "'schema' column \"{}\" expects a child with {error}",
            column.name
        )));
    }
    Ok(())
}

/// What parsing needs from the binder: type names, and casts of the defaults.
pub struct SchemaParser<'a> {
    pub parse_type: &'a mut dyn FnMut(&str) -> Result<LogicalType>,
    pub try_cast: &'a dyn Fn(&Value, &LogicalType) -> Option<Value>,
}

impl SchemaParser<'_> {
    /// `ParseSchemaOption`.
    pub fn parse(&mut self, value: &Value) -> Result<Vec<SchemaColumn>> {
        if value.logical_type().id() != LogicalTypeId::Map {
            return Err(invalid(format!(
                "'schema' expects a value of type MAP, not {}",
                type_id_name(value.logical_type())
            )));
        }
        let key_type = map_key_type(value.logical_type());
        self.parse_map(value, &key_type, true)
    }

    fn parse_map(&mut self, value: &Value, root_key: &LogicalType, root: bool) -> Result<Vec<SchemaColumn>> {
        if value.logical_type().id() != LogicalTypeId::Map {
            let message = format!(
                "'schema' 'children' expects a value of type MAP, not {}",
                type_id_name(value.logical_type())
            );
            return Err(if root { invalid(message) } else { binder_error(message) });
        }
        let entry_type = value
            .logical_type()
            .child_type()
            .cloned()
            .unwrap_or(LogicalType::SQLNULL);
        let entry_fields = entry_type.struct_children();
        let (key_type, value_type) = (entry_fields[0].1.clone(), entry_fields[1].1.clone());
        verify_definition_type(&value_type, root)?;
        if root {
            if !matches!(key_type.id(), LogicalTypeId::Integer | LogicalTypeId::Varchar) {
                return Err(invalid(format!(
                    "'schema' expects the value type of the map to be either INTEGER or VARCHAR, not {}",
                    type_id_name(&key_type)
                )));
            }
        } else if key_type != *root_key {
            return Err(binder_error(format!(
                "'schema' 'children' key type must match the root schema key type {root_key}, not {key_type}"
            )));
        }
        let ValueData::List(entries) = value.data() else {
            return Ok(Vec::new());
        };
        let mut result = Vec::with_capacity(entries.len());
        let mut keys: Vec<SchemaKey> = Vec::new();
        for entry in entries {
            let column = self.parse_definition(entry, root_key)?;
            if keys.contains(&column.key) {
                return Err(invalid("Map keys must be unique."));
            }
            keys.push(column.key.clone());
            result.push(column);
        }
        Ok(result)
    }

    /// `ParseSchemaDefinition`.
    fn parse_definition(&mut self, entry: &Value, root_key: &LogicalType) -> Result<SchemaColumn> {
        let ValueData::Struct(parts) = entry.data() else {
            return Err(binder_error("'schema' definition cannot be NULL"));
        };
        let (key, definition) = (&parts[0], &parts[1]);
        let ValueData::Struct(fields) = definition.data() else {
            return Err(binder_error("'schema' definition cannot be NULL"));
        };
        verify_definition_type(definition.logical_type(), false)?;
        let name = fields[0].try_to_string()?;
        let ty = (self.parse_type)(&fields[1].try_to_string()?)?;
        let default = if fields[2].is_null() {
            Value::null(ty.clone())
        } else {
            (self.try_cast)(&fields[2], &ty).ok_or_else(|| {
                binder_error(format!(
                    "Unable to cast 'schema' default_value \"{}\" to {ty}",
                    fields[2].try_to_string().unwrap_or_default()
                ))
            })?
        };
        let key = match key.data() {
            ValueData::Int32(id) => SchemaKey::FieldId(*id),
            _ => SchemaKey::Name(key.try_to_string()?),
        };
        let mut column = SchemaColumn {
            key,
            name,
            ty,
            default,
            children: Vec::new(),
        };
        if let Some(children) = fields.get(3)
            && !children.is_null()
        {
            column.children = self.parse_map(children, root_key, false)?;
            verify_children(&column)?;
        }
        Ok(column)
    }
}

fn map_key_type(ty: &LogicalType) -> LogicalType {
    ty.child_type()
        .and_then(|entry| entry.struct_children().first().map(|(_, ty)| ty.clone()))
        .unwrap_or(LogicalType::SQLNULL)
}

/// The field id of a column of a file, as arrow keeps it.
pub fn field_id(field: &Field) -> Option<i32> {
    field.metadata().get("PARQUET:field_id").and_then(|id| id.parse().ok())
}

/// The position of the field that `key` names, of `fields`.
pub fn find_field(fields: &[&Field], key: &SchemaKey) -> Option<usize> {
    match key {
        SchemaKey::FieldId(id) => fields.iter().position(|f| field_id(f) == Some(*id)),
        SchemaKey::Name(name) => fields.iter().position(|f| f.name().eq_ignore_ascii_case(name)),
    }
}

/// How a value of a file becomes a value of the result.
#[derive(Debug, Clone)]
pub enum Transform {
    /// a cast to the type of the result, if it differs
    Direct,
    Struct(Vec<ChildMap>),
    List(Box<ChildMap>),
    Map(Box<ChildMap>, Box<ChildMap>),
}

/// A child of a nested column: the child of the file it reads, or its default.
#[derive(Debug, Clone)]
pub enum ChildMap {
    Missing(Value),
    Present {
        index: usize,
        target: LogicalType,
        transform: Transform,
    },
}

fn mismatch(file: &LogicalType, target: &LogicalType) -> Error {
    binder_error(format!(
        "Failed to map file-column of type '{file}' to result-column of type '{target}'"
    ))
}

/// The children of an arrow field, as they hold the field ids of nested columns.
fn arrow_children(field: &Field) -> Vec<&Field> {
    match field.data_type() {
        DataType::Struct(fields) => fields.iter().map(|f| f.as_ref()).collect(),
        DataType::List(child) | DataType::LargeList(child) | DataType::FixedSizeList(child, _) => vec![child.as_ref()],
        DataType::Map(entries, _) => match entries.data_type() {
            DataType::Struct(fields) => fields.iter().map(|f| f.as_ref()).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The transform of a column of `schema` over the file column `field`, of DuckDB type `file_type`.
pub fn build_transform(column: &SchemaColumn, field: &Field, file_type: &LogicalType) -> Result<Transform> {
    // a file column that is not nested (a column of NULLs, say) is cast as it is
    if column.children.is_empty() || named_children(file_type).is_empty() {
        return Ok(Transform::Direct);
    }
    let file_children = arrow_children(field);
    let file_named = named_children(file_type);
    let child_of = |child: &SchemaColumn, found: Option<usize>| -> Result<ChildMap> {
        let Some(index) = found else {
            return Ok(ChildMap::Missing(child.default.clone()));
        };
        let (Some(child_field), Some((_, child_type))) = (file_children.get(index), file_named.get(index)) else {
            return Ok(ChildMap::Missing(child.default.clone()));
        };
        Ok(ChildMap::Present {
            index,
            target: child.ty.clone(),
            transform: build_transform(child, child_field, child_type)?,
        })
    };
    match (column.ty.id(), file_type.id()) {
        (LogicalTypeId::Struct, LogicalTypeId::Struct) => {
            let children = column
                .children
                .iter()
                .map(|child| child_of(child, find_field(&file_children, &child.key)))
                .collect::<Result<_>>()?;
            Ok(Transform::Struct(children))
        }
        (LogicalTypeId::List, LogicalTypeId::List) => {
            let child = &column.children[0];
            // the element of a list is the child of its repeated group, by either name
            let found = match &child.key {
                SchemaKey::Name(name) if matches!(name.as_str(), "list" | "element") => Some(0),
                key => find_field(&file_children, key),
            };
            Ok(Transform::List(Box::new(child_of(child, found)?)))
        }
        (LogicalTypeId::Map, LogicalTypeId::Map) => {
            let find = |child: &SchemaColumn, position: usize| match &child.key {
                SchemaKey::Name(name) if matches!(name.as_str(), "key" | "value") => Some(position),
                key => find_field(&file_children, key),
            };
            let key = child_of(&column.children[0], find(&column.children[0], 0))?;
            let value = child_of(&column.children[1], find(&column.children[1], 1))?;
            Ok(Transform::Map(Box::new(key), Box::new(value)))
        }
        _ => Err(mismatch(file_type, &column.ty)),
    }
}

/// A value of a file, as `transform` makes it a value of `target`.
pub fn apply(
    transform: &Transform,
    value: Value,
    target: &LogicalType,
    cast: &dyn Fn(&Value, &LogicalType) -> Result<Value>,
) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::null(target.clone()));
    }
    let child = |map: &ChildMap, from: Option<&Value>| -> Result<Value> {
        match map {
            ChildMap::Missing(default) => Ok(default.clone()),
            ChildMap::Present { target, transform, .. } => apply(
                transform,
                from.cloned().unwrap_or_else(|| Value::null(target.clone())),
                target,
                cast,
            ),
        }
    };
    match transform {
        Transform::Direct => {
            if value.logical_type() == target {
                Ok(value)
            } else {
                cast(&value, target)
            }
        }
        Transform::Struct(children) => {
            let ValueData::Struct(fields) = value.data() else {
                return Ok(Value::null(target.clone()));
            };
            let values = children
                .iter()
                .map(|map| {
                    let from = match map {
                        ChildMap::Present { index, .. } => fields.get(*index),
                        ChildMap::Missing(_) => None,
                    };
                    child(map, from)
                })
                .collect::<Result<Vec<_>>>()?;
            Value::new(target.clone(), ValueData::Struct(values))
        }
        Transform::List(element) => {
            let ValueData::List(items) = value.data() else {
                return Ok(Value::null(target.clone()));
            };
            let values = items
                .iter()
                .map(|item| child(element, Some(item)))
                .collect::<Result<Vec<_>>>()?;
            Value::new(target.clone(), ValueData::List(values))
        }
        Transform::Map(key, map_value) => {
            let ValueData::List(entries) = value.data() else {
                return Ok(Value::null(target.clone()));
            };
            let entry_type = target.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            let values = entries
                .iter()
                .map(|entry| {
                    let ValueData::Struct(parts) = entry.data() else {
                        return Ok(Value::null(entry_type.clone()));
                    };
                    let k = child(key, parts.first())?;
                    let v = child(map_value, parts.get(1))?;
                    Value::new(entry_type.clone(), ValueData::Struct(vec![k, v]))
                })
                .collect::<Result<Vec<_>>>()?;
            Value::new(target.clone(), ValueData::List(values))
        }
    }
}
