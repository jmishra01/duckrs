// Ported from duckdb/src/planner/binder/statement/bind_alter.cpp, the ALTER side of
// duckdb/src/catalog/catalog_entry/duck_table_entry.cpp and of
// duckdb/src/catalog/dependency_manager.cpp @ rust-port-baseline
//
//! ALTER. Table entries don't change: each ALTER builds the entry that replaces the old one, with
//! an expression per column that says what becomes of the rows. That keeps the old entry intact
//! for a rollback.

use super::expression::checked_cast;
use super::table_constraints::{bind_unique, check_key_types};
use super::{Binder, Binding, binder_error};
use crate::catalog::{
    ColumnDefinition, GeneratedColumn, SchemaCatalogEntry, TableCatalogEntry, TableConstraint, TableOrView,
    ViewCatalogEntry, missing_entry,
};
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    AlterInfo, AlterKind, ColumnDefinition as ParsedColumn, Constraint, Expr, ExprKind, LiteralKind,
    sql_quoted_identifier,
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
    pub generated: Vec<GeneratedColumn>,
    pub constraints: Vec<TableConstraint>,
    /// The new columns, reading the old ones; `None` when the rows stay as they are.
    pub projection: Option<Vec<BoundExpr>>,
    /// The rows have to pass the NOT NULLs and the keys of the new entry.
    pub verify: bool,
    /// The error of an ADD constraint that the rows pass: C++ builds the index first.
    pub rejected: Option<Error>,
    /// What `COMMENT ON TABLE` says of the new entry.
    pub comment: Option<String>,
    /// The column whose type changes, which is written again even to the type it had (`RowGroup::AlterType`).
    pub retyped: Option<usize>,
    /// What a write-ahead log says of it (`WriteAlter`), for the ALTERs of a column that it logs as such rather
    /// than as the table made again.
    pub logged: Option<AlterKind>,
}

/// The columns of `table` and then `column`, as CREATE TABLE would list them.
fn existing_columns(table: &TableCatalogEntry, column: &ParsedColumn) -> Vec<ParsedColumn> {
    let mut parsed: Vec<ParsedColumn> = table
        .logical_columns()
        .iter()
        .map(|c| ParsedColumn {
            name: c.name.to_string(),
            column_type: None,
            generated: c.generated.map(|e| Box::new(e.clone())),
            default_value: None,
            compression: None,
        })
        .collect();
    parsed.push(column.clone());
    parsed
}

/// `ColumnDependencyManager::GetDependents`: the generated columns reading `name`, in any depth.
fn generated_dependents(table: &TableCatalogEntry, name: &str) -> Result<Vec<String>> {
    let mut result: Vec<String> = Vec::new();
    let mut pending = vec![name.to_string()];
    while let Some(next) = pending.pop() {
        for column in &table.generated {
            let reads = super::generated::dependencies(&column.expr)?;
            if reads.iter().any(|r| r.eq_ignore_ascii_case(&next))
                && !result.iter().any(|r| r.eq_ignore_ascii_case(&column.name))
            {
                result.push(column.name.clone());
                pending.push(column.name.clone());
            }
        }
    }
    Ok(result)
}

fn rename_in_generated(generated: &mut [GeneratedColumn], old: &str, new: &str) {
    for column in generated {
        if column.name.eq_ignore_ascii_case(old) {
            column.name = new.to_string();
        }
        visit_expr_mut(&mut column.expr, &mut |e| {
            if let ExprKind::ColumnRef(names) = &mut e.kind
                && names.last().is_some_and(|n| n.eq_ignore_ascii_case(old))
            {
                *names.last_mut().expect("a name") = new.to_string();
            }
        });
    }
}

/// The positions of the generated columns, after dropping `dropped` and its dependents.
fn reposition_generated(generated: &mut [GeneratedColumn], table: &TableCatalogEntry, dropped: &str) {
    let logical = table.logical_columns();
    let keep = |name: &str| {
        !name.eq_ignore_ascii_case(dropped)
            && (generated_contains(table, name, generated) || table.column_index(name).is_some())
    };
    let remaining: Vec<&str> = logical.iter().map(|c| c.name).filter(|n| keep(n)).collect();
    let positions: Vec<usize> = generated
        .iter()
        .map(|c| {
            remaining
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&c.name))
                .unwrap_or(c.position)
        })
        .collect();
    for (column, position) in generated.iter_mut().zip(positions) {
        column.position = position;
    }
}

fn generated_contains(table: &TableCatalogEntry, name: &str, generated: &[GeneratedColumn]) -> bool {
    !table.is_generated(name) || generated.iter().any(|c| c.name.eq_ignore_ascii_case(name))
}

fn catalog_error(message: String) -> Error {
    Error::new(ErrorKind::Catalog, message)
}

fn not_implemented(message: &str) -> Error {
    Error::new(ErrorKind::NotImplemented, message)
}

pub(super) fn column_index(table: &TableCatalogEntry, name: &str) -> Result<usize> {
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
        // `DuckCatalog::Alter` of a schema: its options are not supported
        if let AlterKind::SetSchemaOptions(_) | AlterKind::ResetSchemaOptions(_) = &info.kind {
            let mut path = info.name.qualification.clone();
            path.push(info.name.name.clone());
            if let Err(error) = self.catalog.schema_for(Some(&crate::catalog::schema_key(&path))) {
                if info.if_exists {
                    return Ok(BoundAlter::Nothing);
                }
                return Err(error);
            }
            let verb = if matches!(info.kind, AlterKind::SetSchemaOptions(_)) {
                "SET"
            } else {
                "RESET"
            };
            return Err(not_implemented(&format!(
                "{verb} (<options>) is not supported for DuckDB schemas"
            )));
        }
        let schema = match self
            .catalog
            .schema_with_entry(self.schema_of(&info.name)?.as_deref(), &info.name.name)
        {
            Ok(schema) => schema,
            Err(_) if info.if_exists => return Ok(BoundAlter::Nothing),
            // `Catalog::LookupEntry`: a nested schema path that is missing is named
            Err(error) if info.name.qualification.len() >= 2 && error.message.starts_with("Schema with name") => {
                let schema = info.name.qualification.join(".");
                return Err(catalog_error(format!(
                    "Table with name \"{schema}.{}\" does not exist because schema \"{schema}\" does not exist.",
                    info.name.name
                )));
            }
            Err(error) => return Err(error),
        };
        // the names of a table of a nested schema are looked up in that schema
        if schema.path.len() > 1 && self.search_schema.is_none() {
            self.search_schema = Some(schema.ident());
        }
        let Some(entry) = schema.get_entry(&info.name.name) else {
            // `Binder::Bind(AlterStatement)`: an entry of the system catalog can't change
            let schema_name = self.schema_of(&info.name)?;
            let system_function = matches!(
                &info.kind,
                AlterKind::SetComment { entry_type, .. } if matches!(*entry_type, "FUNCTION" | "MACRO" | "MACRO TABLE")
            ) && (crate::function::BuiltinFunctions::get()
                .scalar(&info.name.name)
                .is_some()
                || crate::function::BuiltinFunctions::get()
                    .aggregate(&info.name.name)
                    .is_some()
                || crate::catalog::defaults::default_macro(schema_name.as_deref(), &info.name.name, false).is_some());
            if system_function
                || crate::catalog::defaults::default_view(schema_name.as_deref(), &info.name.name).is_some()
            {
                return Err(binder_error("Can not comment on System Catalog entries"));
            }
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
        // `ViewCatalogEntry::AlterEntry`: ALTER TABLE ... RENAME TO also renames a view
        let renamed_view = match (&info.kind, &entry) {
            (AlterKind::RenameTable(new_name), TableOrView::View(_)) => Some(new_name),
            (AlterKind::RenameView(new_name), _) => Some(new_name),
            _ => None,
        };
        if let Some(new_name) = renamed_view {
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
            // `Binder::BindAlterAddIndex`
            if matches!(info.kind, AlterKind::AddConstraint(Constraint::Unique { .. })) {
                return Err(binder_error(
                    "Cannot execute the `ALTER TABLE` statement on `View`, only `Table` entries are accepted.",
                ));
            }
            return Err(catalog_error("Can only modify table with ALTER TABLE statement".into()));
        };
        // `DuckTableEntry::RemoveColumn` and `ChangeColumnType`
        let touched = match &info.kind {
            AlterKind::RemoveColumn { path, .. } => Some((path, "drop this column")),
            AlterKind::ChangeColumnType { path, .. } => Some((path, "change the type of this column")),
            _ => None,
        };
        if let Some((path, action)) = touched
            && let [column] = path.as_slice()
            && let Some(index) = table.column_index(column)
        {
            let indexes = table.indexes.read();
            if indexes.iter().any(|i| i.columns.contains(&index)) {
                return Err(catalog_error(format!("Cannot {action}: an index depends on it!")));
            }
            // `DataTable` of a removed column: the columns after it move, also those of the
            // indexes of UNIQUE and PRIMARY KEY constraints (the constraints on the column itself
            // were refused before)
            let mut indexed: Vec<usize> = indexes.iter().flat_map(|i| i.columns.iter().copied()).collect();
            for constraint in &table.constraints {
                if let TableConstraint::Unique { columns, .. } = constraint
                    && !columns.contains(&index)
                {
                    indexed.extend(columns.iter().copied());
                }
            }
            let constrained = table
                .constraints
                .iter()
                .any(|c| matches!(c, TableConstraint::Unique { columns, .. } if columns.contains(&index)));
            if matches!(info.kind, AlterKind::RemoveColumn { .. }) && !constrained && indexed.iter().any(|c| *c > index)
            {
                return Err(catalog_error(
                    "Cannot drop this column: an index depends on a column after it!".to_string(),
                ));
            }
        }
        // `DependencyManager::AlterObject`
        let has_dependents = schema.is_referenced(&table) || !table.indexes.read().is_empty();
        if has_dependents
            && !matches!(
                info.kind,
                AlterKind::AddColumn { .. }
                    | AlterKind::SetDefault { .. }
                    | AlterKind::SetTableOptions(_)
                    | AlterKind::ResetTableOptions(_)
                    | AlterKind::SetComment { .. }
                    | AlterKind::SetColumnComment { .. }
            )
        {
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

    fn bind_new_column(&self, column: &ParsedColumn, schema: &str) -> Result<ColumnDefinition> {
        let Some(ty) = &column.column_type else {
            return Err(crate::unported("columns without a type"));
        };
        // `AddColumn`: the types are looked up in the schema of the table first
        let mut type_binder = self.child_binder();
        type_binder.search_schema = Some(schema.to_string());
        let mut definition = ColumnDefinition::new(column.name.clone(), type_binder.bind_type(ty)?);
        // `CheckTypeIsSupported`
        self.catalog.check_column_type(&definition.ty)?;
        // `AddColumnInfo` keeps the compression, unchecked
        definition.compression = column
            .compression
            .and_then(crate::storage::compression::compression_of_name);
        if let Some(default) = &column.default_value {
            definition.default_value = Some(self.bind_default(default, &definition.ty)?);
            definition.default_text = Some(default.to_string());
        }
        Ok(definition)
    }

    pub(super) fn bind_default(&self, default: &Expr, ty: &LogicalType) -> Result<BoundExpr> {
        let mut binder = Binder::with_options(self.catalog, self.options.clone());
        binder.aggregate_error = "DEFAULT value cannot contain aggregates!";
        binder.search_schema = self.search_schema.clone();
        checked_cast(binder.bind_expression(default)?, ty)
    }

    /// `AlterBinder`: an expression over the rows of the table.
    fn bind_alter_expression(&self, table: &TableCatalogEntry, expression: &Expr) -> Result<BoundExpr> {
        // the parameters of lambdas, which are no columns
        let mut lambda_parameters: Vec<String> = Vec::new();
        visit_expr_mut(&mut expression.clone(), &mut |e: &mut Expr| {
            if let ExprKind::Lambda { lhs, .. } = &e.kind {
                visit_expr_mut(&mut (**lhs).clone(), &mut |p: &mut Expr| {
                    if let ExprKind::ColumnRef(names) = &p.kind
                        && let [name] = names.as_slice()
                    {
                        lambda_parameters.push(name.to_lowercase());
                    }
                });
            }
        });
        let mut error = None;
        visit_expr_mut(&mut expression.clone(), &mut |e: &mut Expr| match &e.kind {
            ExprKind::ColumnRef(names) if names.len() == 1 && lambda_parameters.contains(&names[0].to_lowercase()) => {}
            ExprKind::Subquery { .. } => {
                error.get_or_insert_with(|| "cannot use subquery in alter statement".to_string());
            }
            ExprKind::Window(_) => {
                error.get_or_insert_with(|| "window functions are not allowed in alter statement".to_string());
            }
            // `AlterBinder::BindColumnReference` (`BindQualifiedColumnName`): past a leading table name,
            // the first name is the column
            ExprKind::ColumnRef(names) => {
                let column = match names.as_slice() {
                    [first, second, ..] if first.eq_ignore_ascii_case(&table.name) => second,
                    [first, ..] => first,
                    [] => return,
                };
                if table.is_generated(column) {
                    error.get_or_insert_with(|| "Using generated columns in alter statement not supported".to_string());
                } else if table.column_index(column).is_none() {
                    error.get_or_insert_with(|| {
                        format!(
                            "Table does not contain column {} referenced in alter statement!",
                            sql_quoted_identifier(column)
                        )
                    });
                }
            }
            _ => {}
        });
        if let Some(error) = error {
            return Err(binder_error(error));
        }
        let mut binder = Binder::with_options(self.catalog, self.options.clone());
        binder.aggregate_error = "aggregate functions are not allowed in alter statement";
        let table_index = binder.generate_table_index();
        binder.bind_context.add_binding(Binding {
            alias: table.name.clone(),
            schema: None,
            catalog: None,
            table_index,
            names: table.columns.iter().map(|c| c.name.clone()).collect(),
            types: table.types(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
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
            generated: table.generated.clone(),
            constraints: table.constraints.clone(),
            projection: None,
            verify: false,
            rejected: None,
            comment: table.comment.lock().clone(),
            retyped: None,
            logged: match kind {
                AlterKind::AddColumn { column, unique, .. } if column.generated.is_none() && !*unique => {
                    Some(kind.clone())
                }
                AlterKind::RemoveColumn { path, .. }
                | AlterKind::ChangeColumnType { path, .. }
                | AlterKind::SetDefault { path, .. }
                | AlterKind::SetNotNull(path)
                | AlterKind::DropNotNull(path)
                | AlterKind::RenameColumn { path, .. }
                    if path.len() == 1 =>
                {
                    Some(kind.clone())
                }
                _ => None,
            },
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
            AlterKind::RenameColumn { path, new_name } if path.len() > 1 => {
                return self.rename_field(alter, path, new_name);
            }
            AlterKind::RenameColumn { path, new_name } => {
                if table.is_generated(new_name) {
                    return Err(catalog_error(format!(
                        "Column with name {} already exists!",
                        sql_quoted_identifier(new_name)
                    )));
                }
                let renames_generated = table.is_generated(&path[0]);
                rename_in_generated(&mut alter.generated, &path[0], new_name);
                if renames_generated {
                    return Ok(BoundAlter::Table(alter));
                }
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
                if table.column_index(&column.name).is_some() || table.is_generated(&column.name) {
                    if *if_not_exists {
                        return Ok(BoundAlter::Nothing);
                    }
                    return Err(catalog_error(format!(
                        "Column with name {} already exists!",
                        sql_quoted_identifier(&column.name)
                    )));
                }
                if column.generated.is_some() {
                    let parsed = existing_columns(&table, column);
                    let mut bound = self.bind_generated_columns(&table.name, &parsed, &table.columns)?;
                    let position = parsed.len() - 1;
                    alter.generated.push(
                        bound
                            .drain(..)
                            .find(|c| c.position == position)
                            .expect("the new column is generated"),
                    );
                    return Ok(BoundAlter::Table(alter));
                }
                let mut definition = self.bind_new_column(column, &alter.schema.name)?;
                definition.not_null = *not_null;
                let mut projection = identity(&table)?;
                projection.push(
                    definition
                        .default_value
                        .clone()
                        .unwrap_or_else(|| BoundExpr::constant(Value::null(definition.ty.clone()))),
                );
                // the constraints of the column, as written on it
                if *not_null {
                    alter.constraints.push(TableConstraint::NotNull(alter.columns.len()));
                }
                if *unique {
                    alter.constraints.push(TableConstraint::Unique {
                        columns: vec![alter.columns.len()],
                        primary_key: false,
                        column_level: true,
                        check_mode: duckrs_parser::ast::ConstraintCheckMode::Default,
                    });
                }
                alter.columns.push(definition);
                alter.projection = Some(projection);
                alter.verify = *not_null || *unique;
            }
            AlterKind::RemoveColumn { path, if_exists, .. } if path.len() > 1 => {
                return self.remove_field(alter, path, *if_exists);
            }
            AlterKind::RemoveColumn {
                path,
                if_exists,
                cascade,
            } => {
                let dependents = generated_dependents(&table, &path[0])?;
                if !dependents.is_empty() && !cascade {
                    return Err(catalog_error(
                        "Cannot drop column: column is a dependency of 1 or more generated column(s)".to_string(),
                    ));
                }
                alter
                    .generated
                    .retain(|c| !dependents.iter().any(|d| d.eq_ignore_ascii_case(&c.name)));
                if table.is_generated(&path[0]) {
                    alter.generated.retain(|c| !c.name.eq_ignore_ascii_case(&path[0]));
                    reposition_generated(&mut alter.generated, &table, &path[0]);
                    return Ok(BoundAlter::Table(alter));
                }
                let Some(index) = table.column_index(&path[0]) else {
                    if *if_exists {
                        return Ok(BoundAlter::Nothing);
                    }
                    return Err(column_index(&table, &path[0]).expect_err("the column isn't there"));
                };
                remove_column(&mut alter, index)?;
                reposition_generated(&mut alter.generated, &table, &path[0]);
            }
            AlterKind::ChangeColumnType { path, target, using } => {
                if path.len() > 1 {
                    return Err(not_implemented(
                        "Changing the type of a nested field is not yet supported",
                    ));
                }
                if table.is_generated(&path[0]) {
                    return Err(binder_error("Using generated columns in alter statement not supported"));
                }
                if let Some(dependent) = generated_dependents(&table, &path[0])?.first() {
                    return Err(catalog_error(format!(
                        "This column is referenced by the generated column {dependent}, so its type can not be changed"
                    )));
                }
                let index = column_index(&table, &path[0])?;
                self.change_type(&mut alter, index, target.as_ref(), using.as_deref())?;
                alter.retyped = Some(index);
            }
            AlterKind::SetDefault { path, expression } => {
                if path.len() > 1 {
                    return Err(not_implemented(
                        "Setting a default value on a nested field is not yet supported",
                    ));
                }
                if table.is_generated(&path[0]) {
                    return Err(binder_error(format!(
                        "Cannot SET DEFAULT for generated column {}",
                        sql_quoted_identifier(&path[0])
                    )));
                }
                let index = column_index(&table, &path[0])?;
                let default = match expression {
                    Some(expression) => Some(self.bind_default(expression, &table.columns[index].ty)?),
                    None => None,
                };
                alter.columns[index].default_value = default;
                // what a checkpoint writes and `information_schema` shows
                alter.columns[index].default_text = expression.as_ref().map(ToString::to_string);
            }
            AlterKind::SetNotNull(path) => {
                if path.len() > 1 {
                    return Err(not_implemented(
                        "Setting a NOT NULL constraint on a nested field is not yet supported",
                    ));
                }
                if table.is_generated(&path[0]) {
                    return Err(binder_error("Unsupported constraint for generated column!"));
                }
                let index = column_index(&table, &path[0])?;
                // `DuckTableEntry::SetNotNull`: the constraint goes last, once
                if !alter
                    .constraints
                    .iter()
                    .any(|c| matches!(c, TableConstraint::NotNull(i) if *i == index))
                {
                    alter.constraints.push(TableConstraint::NotNull(index));
                }
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
                    |c| matches!(c, TableConstraint::Unique { columns, primary_key: true, .. } if columns.contains(&index)),
                );
                if in_primary_key {
                    return Err(catalog_error(format!(
                        "column {} is in a primary key",
                        sql_quoted_identifier(&path[0])
                    )));
                }
                alter.columns[index].not_null = false;
                alter
                    .constraints
                    .retain(|c| !matches!(c, TableConstraint::NotNull(i) if *i == index));
            }
            AlterKind::AddConstraint(constraint) => {
                if let Constraint::Unique { check_mode, .. } = constraint
                    && !table.schema.eq_ignore_ascii_case(crate::catalog::TEMP_SCHEMA)
                {
                    self.catalog.check_constraint_check_mode(*check_mode)?;
                }
                add_constraint(&mut alter, constraint)?;
            }
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
            AlterKind::AddField {
                parent,
                column,
                if_not_exists,
            } => return self.add_field(alter, parent, column, *if_not_exists),
            // `SetCommentInfo` and `SetColumnCommentInfo` of a table: the rows stay
            AlterKind::SetComment { value, .. } => {
                alter.comment = (!matches!(value.kind, LiteralKind::Null)).then(|| value.text.clone());
            }
            AlterKind::SetColumnComment { column, value } => {
                let index = column_index(&table, column)?;
                alter.columns[index].comment = (!matches!(value.kind, LiteralKind::Null)).then(|| value.text.clone());
            }
            _ => return Err(crate::unported("this ALTER statement")),
        }
        Ok(BoundAlter::Table(alter))
    }

    /// `DuckTableEntry::ChangeColumnType`.
    pub(super) fn change_type(
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
        // `CheckTypeIsSupported`
        self.catalog.check_column_type(&target)?;
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
            // `UpdateConstraintsOnColumnDrop`: the column's goes with it
            TableConstraint::NotNull(column) => {
                if *column == index {
                    continue;
                }
                *column = shifted(*column);
            }
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
            TableConstraint::Unique {
                columns, primary_key, ..
            } => {
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
        check_mode,
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
    // `BindUniqueConstraint`: a generated column has no values to key on
    if let Some(generated) = columns.iter().find(|name| table.is_generated(name)) {
        let kind = if *primary_key {
            "PRIMARY KEY"
        } else {
            "UNIQUE constraint"
        };
        return Err(binder_error(format!(
            "cannot create a {kind} on a generated column: {}",
            duckrs_parser::ast::sql_identifier(generated)
        )));
    }
    let key = bind_unique(&table.name, &table.columns, *index, columns)?;
    check_key_types(&table.columns, &key)?;
    let quoted = sql_quoted_identifier(&table.name);
    for existing in &table.constraints {
        let TableConstraint::Unique {
            columns: existing,
            primary_key: existing_primary,
            ..
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
    // `BindNewConstraints`: a primary key brings a NOT NULL for each of its columns without one
    let mut implied = Vec::new();
    if *primary_key {
        for column in &key {
            if !alter.columns[*column].not_null {
                implied.push(TableConstraint::NotNull(*column));
            }
            alter.columns[*column].not_null = true;
        }
    }
    alter.constraints.push(TableConstraint::Unique {
        columns: key,
        primary_key: *primary_key,
        column_level: false,
        check_mode: *check_mode,
    });
    alter.constraints.extend(implied);
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
