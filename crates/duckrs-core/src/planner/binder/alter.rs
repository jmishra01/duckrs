// Ported from duckdb/src/planner/binder/statement/bind_alter.cpp, the ALTER side of
// duckdb/src/catalog/catalog_entry/duck_table_entry.cpp and of
// duckdb/src/catalog/dependency_manager.cpp @ rust-port-baseline
//
//! ALTER. Table entries don't change: each ALTER builds the entry that replaces the old one, with
//! an expression per column that says what becomes of the rows. That keeps the old entry intact
//! for a rollback.

use super::expression::checked_cast;
use super::statements::schema_of;
use super::table_constraints::{bind_unique, check_key_types};
use super::{Binder, Binding, binder_error};
use crate::catalog::{
    ColumnDefinition, SchemaCatalogEntry, TableCatalogEntry, TableConstraint, TableOrView, ViewCatalogEntry,
    missing_entry,
};
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    AlterInfo, AlterKind, ColumnDefinition as ParsedColumn, Constraint, Expr, ExprKind, sql_quoted_identifier,
};
use duckrs_parser::transformer::visit_expr_mut;
use std::collections::BTreeSet;
use std::sync::Arc;

/// What an ALTER statement comes down to.
pub enum BoundAlter {
    /// IF EXISTS and IF NOT EXISTS end up here.
    Nothing,
    RenameView {
        schema: Arc<SchemaCatalogEntry>,
        view: Arc<ViewCatalogEntry>,
        new_name: String,
    },
    Table(BoundTableAlter),
}

pub struct BoundTableAlter {
    pub schema: Arc<SchemaCatalogEntry>,
    pub table: Arc<TableCatalogEntry>,
    pub name: String,
    pub columns: Vec<ColumnDefinition>,
    pub constraints: Vec<TableConstraint>,
    /// The new columns, reading the old ones; `None` when the rows stay as they are.
    pub projection: Option<Vec<BoundExpr>>,
    /// The rows have to pass the NOT NULLs and the keys of the new entry.
    pub verify: bool,
    /// The error of an ADD constraint that the rows pass: C++ builds the index first.
    pub rejected: Option<Error>,
}

fn catalog_error(message: String) -> Error {
    Error::new(ErrorKind::Catalog, message)
}

fn not_implemented(message: &str) -> Error {
    Error::new(ErrorKind::NotImplemented, message)
}

fn nested_error(path: &[String]) -> Result<()> {
    if path.len() > 1 {
        return Err(crate::unported("ALTER of nested fields"));
    }
    Ok(())
}

fn column_index(table: &TableCatalogEntry, name: &str) -> Result<usize> {
    table.column_index(name).ok_or_else(|| {
        binder_error(format!(
            "Table {} does not have a column with name {}",
            sql_quoted_identifier(&table.name),
            sql_quoted_identifier(name)
        ))
    })
}

fn referenced_columns(expression: &BoundExpr) -> BTreeSet<usize> {
    let mut columns = BTreeSet::new();
    expression.clone().walk_mut(&mut |e| {
        if let BoundExprKind::Reference(index) = e.kind {
            columns.insert(index);
        }
    });
    columns
}

/// `UniqueConstraint::ToString`.
fn unique_text(table: &TableCatalogEntry, columns: &[usize], primary_key: bool) -> String {
    let names: Vec<&str> = columns.iter().map(|c| table.columns[*c].name.as_str()).collect();
    let kind = if primary_key { "PRIMARY KEY" } else { "UNIQUE" };
    format!("{kind}({})", names.join(", "))
}

/// The columns of the old table, for `BoundTableAlter::projection`.
fn identity(table: &TableCatalogEntry) -> Result<Vec<BoundExpr>> {
    Ok(table
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| BoundExpr {
            kind: BoundExprKind::Reference(index),
            ..BoundExpr::constant(Value::null(column.ty.clone()))
        })
        .collect())
}

impl Binder<'_> {
    /// `Binder::Bind(AlterStatement)` and `AlterEntry`.
    pub fn bind_alter(&mut self, info: &AlterInfo) -> Result<BoundAlter> {
        let schema = match self.catalog.schema_for(schema_of(&info.name)?) {
            Ok(schema) => schema,
            Err(_) if info.if_exists => return Ok(BoundAlter::Nothing),
            Err(error) => return Err(error),
        };
        let Some(entry) = schema.get_entry(&info.name.name) else {
            if info.if_exists {
                return Ok(BoundAlter::Nothing);
            }
            let kind = if matches!(info.kind, AlterKind::RenameView(_)) {
                "View"
            } else {
                "Table"
            };
            return Err(missing_entry(kind, &info.name.name));
        };
        if let AlterKind::RenameView(new_name) = &info.kind {
            let TableOrView::View(view) = entry else {
                return Err(catalog_error("Can only modify view with ALTER VIEW statement".into()));
            };
            Self::check_rename(&schema, &view.name, new_name)?;
            return Ok(BoundAlter::RenameView {
                schema,
                view,
                new_name: new_name.clone(),
            });
        }
        let TableOrView::Table(table) = entry else {
            return Err(catalog_error("Can only modify table with ALTER TABLE statement".into()));
        };
        // `DependencyManager::AlterObject`
        let has_dependents = schema.is_referenced(&table) || !table.indexes.read().is_empty();
        if has_dependents && !matches!(info.kind, AlterKind::AddColumn { .. } | AlterKind::SetDefault { .. }) {
            return Err(Error::new(
                ErrorKind::Dependency,
                format!(
                    "Cannot alter entry {} because there are entries that depend on it.",
                    sql_quoted_identifier(&table.name)
                ),
            ));
        }
        self.bind_table_alter(&info.kind, schema, table)
    }

    fn check_rename(schema: &SchemaCatalogEntry, name: &str, new_name: &str) -> Result<()> {
        if !name.eq_ignore_ascii_case(new_name) && schema.get_entry(new_name).is_some() {
            return Err(catalog_error(format!(
                "Could not rename {} to {}: another entry with this name already exists!",
                sql_quoted_identifier(name),
                sql_quoted_identifier(new_name)
            )));
        }
        Ok(())
    }

    fn bind_new_column(&self, column: &ParsedColumn) -> Result<ColumnDefinition> {
        if column.generated.is_some() {
            return Err(crate::unported("generated columns"));
        }
        let Some(ty) = &column.column_type else {
            return Err(crate::unported("columns without a type"));
        };
        let mut definition = ColumnDefinition::new(column.name.clone(), self.bind_type(ty)?);
        if let Some(default) = &column.default_value {
            definition.default_value = Some(self.bind_default(default, &definition.ty)?);
        }
        Ok(definition)
    }

    fn bind_default(&self, default: &Expr, ty: &LogicalType) -> Result<BoundExpr> {
        let mut binder = Binder::new(self.catalog);
        binder.aggregate_error = "DEFAULT value cannot contain aggregates!";
        checked_cast(binder.bind_expression(default)?, ty)
    }

    /// `AlterBinder`: an expression over the rows of the table.
    fn bind_alter_expression(&self, table: &TableCatalogEntry, expression: &Expr) -> Result<BoundExpr> {
        let mut error = None;
        visit_expr_mut(&mut expression.clone(), &mut |e: &mut Expr| match &e.kind {
            ExprKind::Subquery { .. } => {
                error.get_or_insert_with(|| "cannot use subquery in alter statement".to_string());
            }
            ExprKind::Window(_) => {
                error.get_or_insert_with(|| "window functions are not allowed in alter statement".to_string());
            }
            ExprKind::ColumnRef(names) if names.len() == 1 && table.column_index(&names[0]).is_none() => {
                error.get_or_insert_with(|| {
                    format!(
                        "Table does not contain column {} referenced in alter statement!",
                        sql_quoted_identifier(&names[0])
                    )
                });
            }
            _ => {}
        });
        if let Some(error) = error {
            return Err(binder_error(error));
        }
        let mut binder = Binder::new(self.catalog);
        binder.aggregate_error = "aggregate functions are not allowed in alter statement";
        let table_index = binder.generate_table_index();
        binder.bind_context.add_binding(Binding {
            alias: table.name.clone(),
            schema: None,
            table_index,
            names: table.columns.iter().map(|c| c.name.clone()).collect(),
            types: table.types(),
            row_id: None,
        });
        let mut bound = binder.bind_expression(expression)?;
        let bindings: Vec<ColumnBinding> = (0..table.columns.len())
            .map(|column| ColumnBinding::new(table_index, column))
            .collect();
        resolve_expression(&mut bound, &bindings)?;
        Ok(bound)
    }

    fn bind_table_alter(
        &mut self,
        kind: &AlterKind,
        schema: Arc<SchemaCatalogEntry>,
        table: Arc<TableCatalogEntry>,
    ) -> Result<BoundAlter> {
        let mut alter = BoundTableAlter {
            name: table.name.clone(),
            columns: table.columns.clone(),
            constraints: table.constraints.clone(),
            projection: None,
            verify: false,
            rejected: None,
            schema,
            table: table.clone(),
        };
        match kind {
            AlterKind::RenameTable(new_name) => {
                Self::check_rename(&alter.schema, &table.name, new_name)?;
                rename_in_checks(&mut alter.constraints, |names| {
                    if names.len() == 2 && names[0].eq_ignore_ascii_case(&table.name) {
                        names[0].clone_from(new_name);
                    }
                });
                alter.name.clone_from(new_name);
            }
            AlterKind::RenameColumn { path, new_name } => {
                nested_error(path)?;
                let index = column_index(&table, &path[0])?;
                if table.column_index(new_name).is_some_and(|other| other != index) {
                    return Err(catalog_error(format!(
                        "Column with name {} already exists!",
                        sql_quoted_identifier(new_name)
                    )));
                }
                for constraint in &table.constraints {
                    if let TableConstraint::ForeignKey(key) = constraint {
                        let self_reference = key.referenced.is_none() && key.referenced_columns.contains(&index);
                        if key.columns.contains(&index) || self_reference {
                            return Err(catalog_error(format!(
                                "Cannot rename column {} because this is involved in the foreign key constraint",
                                sql_quoted_identifier(&table.columns[index].name)
                            )));
                        }
                    }
                }
                let old_name = table.columns[index].name.clone();
                rename_in_checks(&mut alter.constraints, |names| {
                    let last = names.len() - 1;
                    let qualified = names.len() == 2 && names[0].eq_ignore_ascii_case(&table.name);
                    if (names.len() == 1 || qualified) && names[last].eq_ignore_ascii_case(&old_name) {
                        names[last].clone_from(new_name);
                    }
                });
                alter.columns[index].name.clone_from(new_name);
            }
            AlterKind::AddColumn {
                column,
                if_not_exists,
                not_null,
                unique,
            } => {
                if table.column_index(&column.name).is_some() {
                    if *if_not_exists {
                        return Ok(BoundAlter::Nothing);
                    }
                    return Err(catalog_error(format!(
                        "Column with name {} already exists!",
                        sql_quoted_identifier(&column.name)
                    )));
                }
                let mut definition = self.bind_new_column(column)?;
                definition.not_null = *not_null;
                let mut projection = identity(&table)?;
                projection.push(
                    definition
                        .default_value
                        .clone()
                        .unwrap_or_else(|| BoundExpr::constant(Value::null(definition.ty.clone()))),
                );
                if *unique {
                    alter.constraints.push(TableConstraint::Unique {
                        columns: vec![alter.columns.len()],
                        primary_key: false,
                    });
                }
                alter.columns.push(definition);
                alter.projection = Some(projection);
                alter.verify = *not_null || *unique;
            }
            AlterKind::RemoveColumn { path, if_exists, .. } => {
                nested_error(path)?;
                let Some(index) = table.column_index(&path[0]) else {
                    if *if_exists {
                        return Ok(BoundAlter::Nothing);
                    }
                    return Err(column_index(&table, &path[0]).expect_err("the column isn't there"));
                };
                remove_column(&mut alter, index)?;
            }
            AlterKind::ChangeColumnType { path, target, using } => {
                if path.len() > 1 {
                    return Err(not_implemented(
                        "Changing the type of a nested field is not yet supported",
                    ));
                }
                let index = column_index(&table, &path[0])?;
                self.change_type(&mut alter, index, target.as_ref(), using.as_deref())?;
            }
            AlterKind::SetDefault { path, expression } => {
                if path.len() > 1 {
                    return Err(not_implemented(
                        "Setting a default value on a nested field is not yet supported",
                    ));
                }
                let index = column_index(&table, &path[0])?;
                let default = match expression {
                    Some(expression) => Some(self.bind_default(expression, &table.columns[index].ty)?),
                    None => None,
                };
                alter.columns[index].default_value = default;
            }
            AlterKind::SetNotNull(path) => {
                if path.len() > 1 {
                    return Err(not_implemented(
                        "Setting a NOT NULL constraint on a nested field is not yet supported",
                    ));
                }
                let index = column_index(&table, &path[0])?;
                alter.columns[index].not_null = true;
                alter.verify = true;
            }
            AlterKind::DropNotNull(path) => {
                if path.len() > 1 {
                    return Err(not_implemented(
                        "Dropping a NOT NULL constraint on a nested field is not yet supported",
                    ));
                }
                let index = column_index(&table, &path[0])?;
                let in_primary_key = table.constraints.iter().any(
                    |c| matches!(c, TableConstraint::Unique { columns, primary_key: true } if columns.contains(&index)),
                );
                if in_primary_key {
                    return Err(catalog_error(format!(
                        "column {} is in a primary key",
                        sql_quoted_identifier(&path[0])
                    )));
                }
                alter.columns[index].not_null = false;
            }
            AlterKind::AddConstraint(constraint) => add_constraint(&mut alter, constraint)?,
            AlterKind::SetPartitionedBy(_) => {
                return Err(not_implemented("SET PARTITIONED BY is not supported for DuckDB tables"));
            }
            AlterKind::SetSortedBy(_) => {
                return Err(not_implemented("SET SORTED BY is not supported for DuckDB tables"));
            }
            AlterKind::SetTableOptions(_) => {
                return Err(not_implemented("SET (<options>) is not supported for DuckDB tables"));
            }
            AlterKind::ResetTableOptions(_) => {
                return Err(not_implemented("RESET (<options>) is not supported for DuckDB tables"));
            }
            AlterKind::AddField { .. } => return Err(crate::unported("ALTER of nested fields")),
            _ => return Err(crate::unported("this ALTER statement")),
        }
        Ok(BoundAlter::Table(alter))
    }

    /// `DuckTableEntry::ChangeColumnType`.
    fn change_type(
        &self,
        alter: &mut BoundTableAlter,
        index: usize,
        target: Option<&duckrs_parser::ast::CastType>,
        using: Option<&Expr>,
    ) -> Result<()> {
        let table = alter.table.clone();
        let target = target.map(|ty| self.bind_type(ty)).transpose()?;
        let source = BoundExpr {
            kind: BoundExprKind::Reference(index),
            ..BoundExpr::constant(Value::null(table.columns[index].ty.clone()))
        };
        let (expression, target) = match using {
            Some(using) => {
                let bound = self.bind_alter_expression(&table, using)?;
                let target = target.unwrap_or_else(|| bound.return_type.clone());
                (bound.cast_to(&target, false), target)
            }
            None => {
                let target = target.expect("the parser wants a type or USING");
                (source.cast_to(&target, false), target)
            }
        };
        for constraint in &table.constraints {
            match constraint {
                TableConstraint::Check { expression, .. } if referenced_columns(expression).contains(&index) => {
                    return Err(binder_error(
                        "Cannot change the type of a column that has a CHECK constraint specified",
                    ));
                }
                TableConstraint::Unique { columns, .. } if columns.contains(&index) => {
                    return Err(binder_error(
                        "Cannot change the type of a column that has a UNIQUE or PRIMARY KEY constraint specified",
                    ));
                }
                TableConstraint::ForeignKey(key) if key.columns.contains(&index) => {
                    return Err(binder_error(
                        "Cannot change the type of a column that has a FOREIGN KEY constraint specified",
                    ));
                }
                _ => {}
            }
        }
        let mut projection = identity(&table)?;
        projection[index] = expression;
        alter.columns[index].ty = target.clone();
        if let Some(default) = alter.columns[index].default_value.take() {
            alter.columns[index].default_value = Some(default.cast_to(&target, false));
        }
        alter.projection = Some(projection);
        alter.verify = alter.columns[index].not_null;
        Ok(())
    }
}

/// `UpdateConstraintsOnColumnDrop`.
fn remove_column(alter: &mut BoundTableAlter, index: usize) -> Result<()> {
    let table = alter.table.clone();
    let name = table.columns[index].name.clone();
    if table.columns.len() == 1 {
        return Err(catalog_error(
            "Cannot drop column: table only has one column remaining!".to_string(),
        ));
    }
    let quoted = sql_quoted_identifier(&name);
    let shifted = |column: usize| if column > index { column - 1 } else { column };
    let mut constraints = Vec::with_capacity(table.constraints.len());
    for constraint in &table.constraints {
        let mut constraint = constraint.clone();
        match &mut constraint {
            TableConstraint::Check { expression, .. } => {
                let columns = referenced_columns(expression);
                if columns.contains(&index) {
                    if columns.len() > 1 {
                        return Err(catalog_error(format!(
                            "Cannot drop column {quoted} because there is a CHECK constraint that depends on it"
                        )));
                    }
                    continue;
                }
                expression.walk_mut(&mut |e| {
                    if let BoundExprKind::Reference(column) = &mut e.kind {
                        *column = shifted(*column);
                    }
                });
            }
            TableConstraint::Unique { columns, primary_key } => {
                if columns.contains(&index) {
                    if columns.len() == 1 {
                        let kind = if *primary_key { "PRIMARY KEY" } else { "UNIQUE" };
                        return Err(catalog_error(format!(
                            "Cannot drop column {quoted} because there is a {kind} constraint that depends on it"
                        )));
                    }
                    let kind = if *primary_key { "primary key" } else { "unique" };
                    return Err(catalog_error(format!(
                        "Cannot drop column {quoted} because it is referenced in {kind} constraint {}",
                        unique_text(&table, columns, *primary_key)
                    )));
                }
                for column in columns.iter_mut() {
                    *column = shifted(*column);
                }
            }
            TableConstraint::ForeignKey(key) => {
                let self_reference = key.referenced.is_none() && key.referenced_columns.contains(&index);
                if key.columns.contains(&index) || self_reference {
                    return Err(catalog_error(format!(
                        "Cannot drop column {quoted} because there is a FOREIGN KEY constraint that depends on it"
                    )));
                }
                for column in &mut key.columns {
                    *column = shifted(*column);
                }
                if key.referenced.is_none() {
                    for column in &mut key.referenced_columns {
                        *column = shifted(*column);
                    }
                }
            }
        }
        constraints.push(constraint);
    }
    let mut projection = identity(&table)?;
    projection.remove(index);
    alter.columns.remove(index);
    alter.constraints = constraints;
    alter.projection = Some(projection);
    Ok(())
}

/// `DuckTableEntry::AddConstraint`.
fn add_constraint(alter: &mut BoundTableAlter, constraint: &Constraint) -> Result<()> {
    let table = alter.table.clone();
    let Constraint::Unique {
        index,
        columns,
        primary_key,
        ..
    } = constraint
    else {
        let kind = if matches!(constraint, Constraint::Check(_)) {
            "CHECK"
        } else {
            "FOREIGN_KEY"
        };
        return Err(not_implemented(&format!(
            "No support for adding {kind} constraints with ALTER TABLE"
        )));
    };
    let key = bind_unique(&table.name, &table.columns, *index, columns)?;
    check_key_types(&table.columns, &key)?;
    let quoted = sql_quoted_identifier(&table.name);
    for existing in &table.constraints {
        let TableConstraint::Unique {
            columns: existing,
            primary_key: existing_primary,
        } = existing
        else {
            continue;
        };
        if *primary_key && *existing_primary {
            alter.rejected.get_or_insert_with(|| {
                catalog_error(format!(
                    "table {quoted} can have only one primary key: {}",
                    unique_text(&table, existing, true)
                ))
            });
        }
        if existing_primary == primary_key && *existing == key {
            alter.rejected.get_or_insert_with(|| {
                catalog_error(format!(
                    "table {quoted} already has the constraint {}",
                    unique_text(&table, existing, *primary_key)
                ))
            });
        }
    }
    if *primary_key {
        for column in &key {
            alter.columns[*column].not_null = true;
        }
    }
    alter.constraints.push(TableConstraint::Unique {
        columns: key,
        primary_key: *primary_key,
    });
    alter.verify = true;
    Ok(())
}

/// Applies `rename` to the column references of the CHECKs, which show in their errors.
fn rename_in_checks(constraints: &mut [TableConstraint], rename: impl Fn(&mut Vec<String>)) {
    for constraint in constraints {
        if let TableConstraint::Check { parsed, text, .. } = constraint {
            visit_expr_mut(parsed, &mut |e: &mut Expr| {
                if let ExprKind::ColumnRef(names) = &mut e.kind {
                    rename(names);
                }
            });
            *text = Constraint::Check(Box::new(parsed.clone())).to_string();
        }
    }
}
