// Ported from duckdb/src/catalog/catalog_entry/duck_table_entry.cpp (AddField, RemoveField,
// RenameField, RemapStructDefault) @ rust-port-baseline
//
//! ALTER of the fields of a STRUCT column, also inside LISTs and MAPs: the column changes type, its
//! rows are rewritten by `remap_struct`, and so is its DEFAULT when a field is added or renamed.
//! C++ builds the `remap_struct` call as a parsed expression from constant values; here it is the
//! SQL of that expression, which is also the text a DEFAULT is stored with.

use super::alter::BoundAlter;
use super::alter::BoundTableAlter;
use super::{Binder, binder_error};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{self, ColumnDefinition as ParsedColumn, Expr};

fn catalog_error(message: String) -> Error {
    Error::new(ErrorKind::Catalog, message)
}

/// An identifier as C++ formats an `Identifier` in a message.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A string literal.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `GetChildList`: the members a mapping walks through.
fn child_list(ty: &LogicalType) -> Result<Vec<(String, LogicalType)>> {
    match ty.id() {
        LogicalTypeId::List => Ok(vec![(
            "list".to_string(),
            ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL),
        )]),
        LogicalTypeId::Map => {
            let entry = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
            Ok(entry.struct_children().to_vec())
        }
        LogicalTypeId::Struct => Ok(ty.struct_children().to_vec()),
        _ => Err(binder_error(format!("Can't ConstructMapping for type '{ty}'"))),
    }
}

/// `LogicalType::ConstructNestedType`.
fn nested_type(ty: &LogicalType, children: Vec<(String, LogicalType)>) -> LogicalType {
    match ty.id() {
        LogicalTypeId::List => LogicalType::list(children.into_iter().next().expect("one child").1),
        LogicalTypeId::Map => {
            let mut children = children.into_iter();
            let key = children.next().expect("a key").1;
            let value = children.next().expect("a value").1;
            LogicalType::map(key, value)
        }
        _ => LogicalType::struct_type(children),
    }
}

fn is_nested(ty: &LogicalType) -> bool {
    ty.is_nested()
}

/// A struct literal of the entries of a mapping.
fn struct_sql(entries: &[(String, String)]) -> String {
    let fields: Vec<String> = entries
        .iter()
        .map(|(name, value)| format!("{}: {value}", literal(name)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// The mapping of a member: its name, or for a nested one `(name, mapping of its members)`.
fn member_mapping(name: &str, ty: &LogicalType, mapping: String) -> String {
    if is_nested(ty) {
        format!("row({}, {mapping})", literal(name))
    } else {
        mapping
    }
}

/// `ConstructMapping`: what maps each member of `ty` to itself.
fn construct_mapping(name: &str, ty: &LogicalType) -> Result<String> {
    if !is_nested(ty) {
        return Ok(literal(name));
    }
    let mut entries = Vec::new();
    for (child, child_type) in child_list(ty)? {
        let mapping = construct_mapping(&child, &child_type)?;
        entries.push((child.clone(), member_mapping(&child, &child_type, mapping)));
    }
    Ok(struct_sql(&entries))
}

/// What a field change gives: the new type, the mapping, the defaults, or an error that IF (NOT)
/// EXISTS may swallow.
struct Remapping {
    new_type: LogicalType,
    mapping: String,
    defaults: Option<String>,
}

/// `PackExpression`.
fn pack(expression: String, name: &str) -> String {
    format!("struct_pack({} := {expression})", quoted(name))
}

/// `AddFieldToStruct`.
fn add_field_to_struct(
    ty: &LogicalType,
    path: &[String],
    name: &str,
    field_type: &LogicalType,
    default: &str,
    depth: usize,
) -> Result<std::result::Result<(LogicalType, String), Error>> {
    if !is_nested(ty) {
        return Err(binder_error(format!(
            "Column {} is not a nested type, ADD COLUMN can only be used on nested types",
            quoted(&path[depth])
        )));
    }
    let mut children = child_list(ty)?;
    let current = &path[depth];
    if depth + 1 == path.len() {
        if ty.id() != LogicalTypeId::Struct {
            return Err(binder_error(format!(
                "Column {} is not a struct - ALTER TABLE can only add fields to structs",
                quoted(current)
            )));
        }
        if children.iter().any(|(child, _)| child.eq_ignore_ascii_case(name)) {
            return Ok(Err(catalog_error(format!(
                "Duplicate field {} - field already exists in struct {}",
                quoted(name),
                quoted(current)
            ))));
        }
        children.push((name.to_string(), field_type.clone()));
        return Ok(Ok((
            LogicalType::struct_type(children),
            pack(default.to_string(), name),
        )));
    }
    let next = &path[depth + 1];
    let list = ty.id() == LogicalTypeId::List;
    for (child, child_type) in children.iter_mut() {
        if (list && next.eq_ignore_ascii_case("element")) || child.eq_ignore_ascii_case(next) {
            let (new_type, defaults) =
                match add_field_to_struct(child_type, path, name, field_type, default, depth + 1)? {
                    Ok(result) => result,
                    Err(error) => return Ok(Err(error)),
                };
            *child_type = new_type;
            let defaults = pack(defaults, if list { "list" } else { child });
            return Ok(Ok((nested_type(ty, children), defaults)));
        }
    }
    Err(binder_error(format!(
        "Sub-field {} does not exist in column {}",
        quoted(next),
        quoted(current)
    )))
}

/// What happens to the field at the end of the path.
enum FieldChange<'a> {
    Drop,
    Rename(&'a str),
}

/// `DropFieldFromStruct` and `RenameFieldFromStruct`.
fn change_field(
    ty: &LogicalType,
    path: &[String],
    change: &FieldChange,
    depth: usize,
) -> Result<std::result::Result<(LogicalType, String), Error>> {
    if !is_nested(ty) {
        let verb = match change {
            FieldChange::Drop => "drop",
            FieldChange::Rename(_) => "rename",
        };
        return Err(catalog_error(format!(
            "Cannot {verb} field from column {} - not a nested type",
            quoted(&path[0])
        )));
    }
    let entry = &path[depth];
    let last = depth + 1 == path.len();
    let list = ty.id() == LogicalTypeId::List;
    let children = child_list(ty)?;
    let mut found = false;
    let mut mapping = Vec::new();
    let mut new_children = Vec::new();
    for (child, child_type) in &children {
        let mut field_name = child.clone();
        let (child_mapping, new_type) = if (list && entry.eq_ignore_ascii_case("element"))
            || child.eq_ignore_ascii_case(entry)
        {
            found = true;
            if last {
                if ty.id() != LogicalTypeId::Struct {
                    return Err(catalog_error(match change {
                        FieldChange::Drop => format!(
                            "Cannot drop field {} from column {} - it's not a struct",
                            quoted(path.last().expect("a path")),
                            quoted(&path[0])
                        ),
                        FieldChange::Rename(_) => format!(
                            "Cannot rename field {} from column {} - can only rename fields inside a struct",
                            quoted(path.last().expect("a path")),
                            quoted(&path[0])
                        ),
                    }));
                }
                match change {
                    FieldChange::Drop => {
                        if children.len() == 1 {
                            return Err(catalog_error(format!(
                                "Cannot drop field {} from column {} - it is the last field of the struct",
                                path.last().expect("a path"),
                                path[0]
                            )));
                        }
                        continue;
                    }
                    FieldChange::Rename(new_name) => {
                        if children.iter().any(|(other, _)| other.eq_ignore_ascii_case(new_name)) {
                            return Err(catalog_error(format!(
                                "Cannot rename field {} from column {} to {} - a field with this name already exists",
                                quoted(path.last().expect("a path")),
                                quoted(&path[0]),
                                quoted(new_name)
                            )));
                        }
                        field_name = new_name.to_string();
                        (construct_mapping(child, child_type)?, child_type.clone())
                    }
                }
            } else {
                match change_field(child_type, path, change, depth + 1)? {
                    Ok((new_type, child_mapping)) => (child_mapping, new_type),
                    Err(error) => return Ok(Err(error)),
                }
            }
        } else {
            (construct_mapping(child, child_type)?, child_type.clone())
        };
        mapping.push((field_name.clone(), member_mapping(child, child_type, child_mapping)));
        new_children.push((field_name, new_type));
    }
    if !found {
        let verb = match change {
            FieldChange::Drop => "drop",
            FieldChange::Rename(_) => "rename",
        };
        return Ok(Err(catalog_error(format!(
            "Cannot {verb} field {} - it does not exist",
            quoted(entry)
        ))));
    }
    Ok(Ok((nested_type(ty, new_children), struct_sql(&mapping))))
}

/// The expression of `sql`.
pub(super) fn parse_expression(sql: &str) -> Result<Expr> {
    let statements = duckrs_parser::parse_ast(&format!("SELECT {sql}"))?;
    if let [ast::Statement::Select(select)] = statements.as_slice()
        && let ast::QueryNodeKind::Select(node) = &select.node.kind
        && let [expr] = node.select_list.as_slice()
    {
        return Ok(expr.clone());
    }
    Err(Error::new(ErrorKind::Internal, format!("not an expression: {sql}")))
}

impl Binder<'_> {
    /// `DuckTableEntry::AddField`.
    pub(super) fn add_field(
        &self,
        alter: BoundTableAlter,
        parent: &[String],
        column: &ParsedColumn,
        if_not_exists: bool,
    ) -> Result<BoundAlter> {
        let table = alter.table.clone();
        let index = super::alter::column_index(&table, &parent[0])?;
        let Some(ty) = &column.column_type else {
            return Err(crate::unported("columns without a type"));
        };
        let field_type = self.bind_type(ty)?;
        let default = match &column.default_value {
            Some(default) => default.to_string(),
            None => format!("CAST(NULL AS {field_type})"),
        };
        let column_type = table.columns[index].ty.clone();
        let (new_type, defaults) =
            match add_field_to_struct(&column_type, parent, &column.name, &field_type, &default, 0)? {
                Ok(result) => result,
                Err(_) if if_not_exists => return Ok(BoundAlter::Nothing),
                Err(error) => return Err(error),
            };
        let mapping = construct_mapping(&table.columns[index].name, &column_type)?;
        self.remap_column(
            alter,
            index,
            Remapping {
                new_type,
                mapping,
                defaults: Some(defaults),
            },
            true,
        )
    }

    /// `DuckTableEntry::RemoveField`.
    pub(super) fn remove_field(&self, alter: BoundTableAlter, path: &[String], if_exists: bool) -> Result<BoundAlter> {
        let table = alter.table.clone();
        let Some(index) = table.column_index(&path[0]) else {
            if if_exists {
                return Ok(BoundAlter::Nothing);
            }
            return Err(catalog_error(format!(
                "Cannot drop field from column {} - it does not exist",
                quoted(&path[0])
            )));
        };
        let column_type = table.columns[index].ty.clone();
        let (new_type, mapping) = match change_field(&column_type, path, &FieldChange::Drop, 1)? {
            Ok(result) => result,
            Err(_) if if_exists => return Ok(BoundAlter::Nothing),
            Err(error) => return Err(error),
        };
        self.remap_column(
            alter,
            index,
            Remapping {
                new_type,
                mapping,
                defaults: None,
            },
            false,
        )
    }

    /// `DuckTableEntry::RenameField`.
    pub(super) fn rename_field(&self, alter: BoundTableAlter, path: &[String], new_name: &str) -> Result<BoundAlter> {
        let table = alter.table.clone();
        let Some(index) = table.column_index(&path[0]) else {
            return Err(catalog_error(format!(
                "Cannot rename field from column {} - it does not exist",
                quoted(&path[0])
            )));
        };
        let column_type = table.columns[index].ty.clone();
        let (new_type, mapping) = change_field(&column_type, path, &FieldChange::Rename(new_name), 1)??;
        self.remap_column(
            alter,
            index,
            Remapping {
                new_type,
                mapping,
                defaults: None,
            },
            true,
        )
    }

    /// `ChangeColumnType` with `remap_struct(column, new_type, mapping, defaults)`; the DEFAULT of
    /// the column is remapped as well when `remap_default` (`RemapStructDefault`).
    fn remap_column(
        &self,
        mut alter: BoundTableAlter,
        index: usize,
        remapping: Remapping,
        remap_default: bool,
    ) -> Result<BoundAlter> {
        let column = alter.table.columns[index].clone();
        let old_type = column.ty.clone();
        let tail = format!(
            "CAST(NULL AS {}), {}, {}",
            remapping.new_type,
            remapping.mapping,
            remapping.defaults.as_deref().unwrap_or("NULL")
        );
        let using = parse_expression(&format!(
            "remap_struct({}, {tail})",
            duckrs_parser::ast::sql_quoted_identifier(&column.name)
        ))?;
        let target = ast::CastType::Resolved(remapping.new_type.clone());
        self.change_type(&mut alter, index, Some(&target), Some(&using))?;
        if remap_default && let Some(text) = &column.default_text {
            // the stored default is not cast yet, the mapping is for the old type of the column
            let remapped = format!("remap_struct(CAST(({text}) AS {old_type}), {tail})");
            let parsed = parse_expression(&remapped)?;
            let bound = self.bind_default(&parsed, &remapping.new_type)?;
            let definition = &mut alter.columns[index];
            definition.default_value = Some(bound);
            definition.default_text = Some(parsed.to_string());
        }
        Ok(BoundAlter::Table(Box::new(alter)))
    }
}
