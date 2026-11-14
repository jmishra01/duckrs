// Ported from duckdb/src/planner/binder/statement/bind_create_table.cpp and
// duckdb/src/planner/expression_binder/check_binder.cpp @ rust-port-baseline
//
//! Binds the constraints of CREATE TABLE.

use super::expression::checked_cast;
use super::{Binder, Binding, binder_error};
use crate::catalog::{ColumnDefinition, ForeignKey, ReferencedTable, SchemaCatalogEntry, TableConstraint, TableOrView};
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::{BoundExpr, ColumnBinding};
use duckrs_common::types::{LogicalType, PhysicalType};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{Constraint, ConstraintCheckMode, CreateTableInfo, Expr, ExprKind, sql_quoted_identifier};
use duckrs_parser::transformer::visit_expr_mut;
use std::sync::Arc;

fn column_index(columns: &[ColumnDefinition], name: &str) -> Option<usize> {
    columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
}

/// `BindUniqueConstraint`.
pub(super) fn bind_unique(
    table: &str,
    columns: &[ColumnDefinition],
    index: Option<usize>,
    names: &[String],
) -> Result<Vec<usize>> {
    if let Some(index) = index {
        return Ok(vec![index]);
    }
    let mut indexes = Vec::with_capacity(names.len());
    for name in names {
        let Some(index) = column_index(columns, name) else {
            return Err(Error::new(
                ErrorKind::Catalog,
                format!(
                    "table {} does not have a column named {}",
                    sql_quoted_identifier(table),
                    sql_quoted_identifier(name)
                ),
            ));
        };
        if indexes.contains(&index) {
            return Err(Error::new(
                ErrorKind::Parser,
                format!("column {name} appears twice in primary key constraint"),
            ));
        }
        indexes.push(index);
    }
    Ok(indexes)
}

/// What `ART::ART` refuses as the type of a key.
pub(super) fn check_key_types(columns: &[ColumnDefinition], key: &[usize]) -> Result<()> {
    for index in key {
        let ty = &columns[*index].ty;
        if matches!(
            ty.physical_type(),
            PhysicalType::List | PhysicalType::Array | PhysicalType::Struct | PhysicalType::Interval
        ) {
            return Err(Error::new(
                ErrorKind::InvalidType,
                format!("Invalid Type [{ty}]: Invalid type for index key."),
            ));
        }
    }
    Ok(())
}

/// `FindMatchingPrimaryKeyColumns`: the key that `referenced` names among `uniques`.
fn find_referenced_key(
    table: &str,
    columns: &[ColumnDefinition],
    uniques: &[(Vec<usize>, bool)],
    foreign: &[String],
    referenced: &[String],
) -> Result<Vec<usize>> {
    let find_primary_key = referenced.is_empty();
    if !find_primary_key && referenced.len() != foreign.len() {
        return Err(binder_error(format!(
            "Failed to create foreign key: number of referencing ({}) and referenced columns ({}) differ",
            foreign.join(","),
            referenced.join(",")
        )));
    }
    let mut found_constraint = false;
    for (key, primary_key) in uniques {
        if find_primary_key && !primary_key {
            continue;
        }
        found_constraint = true;
        if find_primary_key {
            if key.len() != foreign.len() {
                let names: Vec<&str> = key.iter().map(|c| columns[*c].name.as_str()).collect();
                return Err(binder_error(format!(
                    "Failed to create foreign key: number of referencing ({}) and referenced columns ({}) differ",
                    foreign.join(","),
                    names.join(",")
                )));
            }
            return Ok(key.clone());
        }
        let wanted: Vec<Option<usize>> = referenced.iter().map(|name| column_index(columns, name)).collect();
        if key.len() == wanted.len() && key.iter().all(|c| wanted.contains(&Some(*c))) {
            return Ok(wanted.into_iter().flatten().collect());
        }
    }
    if !found_constraint {
        let term = if find_primary_key {
            "primary key"
        } else {
            "primary key or unique constraint"
        };
        return Err(binder_error(format!(
            "Failed to create foreign key: there is no {term} for referenced table {}",
            sql_quoted_identifier(table)
        )));
    }
    for name in referenced {
        if column_index(columns, name).is_none() {
            return Err(binder_error(format!(
                "Failed to create foreign key: referenced table {table} does not have a column named {name}"
            )));
        }
    }
    Err(binder_error(format!(
        "Failed to create foreign key: referenced table {table} does not have a primary key or unique constraint on \
         the columns {}",
        referenced.join(",")
    )))
}

impl Binder<'_> {
    /// `BindCheckConstraint`: the expression reads the columns of the new table by position.
    fn bind_check(&self, table: &str, columns: &[ColumnDefinition], check: &Expr) -> Result<BoundExpr> {
        let mut error = None;
        let mut parsed = check.clone();
        visit_expr_mut(&mut parsed, &mut |expr: &mut Expr| match &expr.kind {
            ExprKind::Subquery { .. } => {
                error.get_or_insert_with(|| "cannot use subquery in check constraint".to_string());
            }
            ExprKind::ColumnRef(names) if names.len() == 1 && column_index(columns, &names[0]).is_none() => {
                error.get_or_insert_with(|| {
                    format!(
                        "Table does not contain column {} referenced in check constraint!",
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
        binder.aggregate_error = "aggregate functions are not allowed in check constraints";
        let table_index = binder.generate_table_index();
        binder.bind_context.add_binding(Binding {
            alias: table.to_string(),
            schema: None,
            table_index,
            names: columns.iter().map(|c| c.name.clone()).collect(),
            types: columns.iter().map(|c| c.ty.clone()).collect(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let mut bound = checked_cast(binder.bind_expression(check)?, &LogicalType::INTEGER)?;
        let bindings: Vec<ColumnBinding> = (0..columns.len())
            .map(|column_index| ColumnBinding {
                table_index,
                column_index,
            })
            .collect();
        resolve_expression(&mut bound, &bindings)?;
        Ok(bound)
    }

    /// `Binder::BindConstraints` and `BindCreateTableConstraints`.
    pub(super) fn bind_table_constraints(
        &mut self,
        info: &CreateTableInfo,
        columns: &mut [ColumnDefinition],
        schema: &Arc<SchemaCatalogEntry>,
    ) -> Result<Vec<TableConstraint>> {
        let table = &info.name.name;
        let mut bound = Vec::new();
        // the keys go first, for the foreign keys of a table that references itself
        let mut has_primary_key = false;
        for constraint in &info.constraints {
            match constraint {
                Constraint::NotNull(index) => columns[*index].not_null = true,
                Constraint::Unique {
                    index,
                    columns: names,
                    primary_key,
                    check_mode,
                } => {
                    if *check_mode == ConstraintCheckMode::Deferred {
                        return Err(crate::unported("DEFERRED constraints"));
                    }
                    if *primary_key && std::mem::replace(&mut has_primary_key, true) {
                        return Err(Error::new(
                            ErrorKind::Parser,
                            format!("table {} has more than one primary key", sql_quoted_identifier(table)),
                        ));
                    }
                    let key = bind_unique(table, columns, *index, names)?;
                    check_key_types(columns, &key)?;
                    if *primary_key {
                        for column in &key {
                            columns[*column].not_null = true;
                        }
                    }
                    bound.push(TableConstraint::Unique {
                        columns: key,
                        primary_key: *primary_key,
                    });
                }
                // `BindForeignKey`, in the order of the constraints: a key named twice
                Constraint::ForeignKey { fk_columns, pk_columns, .. } => {
                    let twice = |names: &[String]| {
                        names
                            .iter()
                            .enumerate()
                            .any(|(at, name)| names[..at].iter().any(|n| n.eq_ignore_ascii_case(name)))
                    };
                    if twice(pk_columns) {
                        return Err(Error::new(
                            ErrorKind::Parser,
                            "duplicate primary key referenced in FOREIGN KEY constraint",
                        ));
                    }
                    if twice(fk_columns) {
                        return Err(Error::new(
                            ErrorKind::Parser,
                            "duplicate key specified in FOREIGN KEY constraint",
                        ));
                    }
                }
                Constraint::Check(_) => {}
            }
        }
        for constraint in &info.constraints {
            match constraint {
                Constraint::Check(check) => bound.push(TableConstraint::Check {
                    expression: self.bind_check(table, columns, check)?,
                    parsed: (**check).clone(),
                    text: constraint.to_string(),
                }),
                Constraint::ForeignKey {
                    fk_columns,
                    pk_columns,
                    schema: referenced_schema,
                    table: referenced_table,
                } => {
                    let foreign_key = self.bind_foreign_key(
                        info,
                        columns,
                        &bound,
                        schema,
                        fk_columns,
                        pk_columns,
                        referenced_schema,
                        referenced_table,
                    )?;
                    bound.push(TableConstraint::ForeignKey(foreign_key));
                }
                Constraint::NotNull(_) | Constraint::Unique { .. } => {}
            }
        }
        Ok(bound)
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_foreign_key(
        &self,
        info: &CreateTableInfo,
        columns: &[ColumnDefinition],
        bound: &[TableConstraint],
        schema: &Arc<SchemaCatalogEntry>,
        fk_columns: &[String],
        pk_columns: &[String],
        referenced_schema: &str,
        referenced_table: &str,
    ) -> Result<ForeignKey> {
        let mut foreign = Vec::with_capacity(fk_columns.len());
        for name in fk_columns {
            let Some(index) = column_index(columns, name) else {
                return Err(binder_error(format!("column {name} named in key does not exist")));
            };
            if foreign.contains(&index) {
                return Err(Error::new(
                    ErrorKind::Parser,
                    "duplicate key specified in FOREIGN KEY constraint",
                ));
            }
            foreign.push(index);
        }
        let uniques_of = |constraints: &[TableConstraint]| -> Vec<(Vec<usize>, bool)> {
            constraints
                .iter()
                .filter_map(|c| match c {
                    TableConstraint::Unique { columns, primary_key } => Some((columns.clone(), *primary_key)),
                    _ => None,
                })
                .collect()
        };
        let same_schema = referenced_schema.is_empty() || referenced_schema.eq_ignore_ascii_case(&schema.name);
        let parent = if same_schema && referenced_table.eq_ignore_ascii_case(&info.name.name) {
            None
        } else {
            let holder = if same_schema {
                schema.clone()
            } else {
                self.catalog.schema_for(Some(referenced_schema))?
            };
            let entry = holder.get_entry(referenced_table);
            match entry {
                Some(TableOrView::Table(parent)) if same_schema => Some(parent),
                Some(TableOrView::Table(_)) => {
                    return Err(binder_error(
                        "Creating foreign keys across different schemas or catalogs is not supported",
                    ));
                }
                Some(_) => return Err(binder_error("cannot reference a VIEW with a FOREIGN KEY")),
                None => return Err(crate::catalog::missing_entry("Table", referenced_table)),
            }
        };
        let (parent_columns, uniques) = match &parent {
            Some(parent) => (parent.columns.as_slice(), uniques_of(&parent.constraints)),
            None => (columns, uniques_of(bound)),
        };
        let wanted = find_referenced_key(referenced_table, parent_columns, &uniques, fk_columns, pk_columns)?;
        for window in 0..wanted.len() {
            if wanted[..window].contains(&wanted[window]) {
                return Err(Error::new(
                    ErrorKind::Parser,
                    "duplicate primary key referenced in FOREIGN KEY constraint",
                ));
            }
        }
        for (pk, fk) in wanted.iter().zip(&foreign) {
            let (pk, fk) = (&parent_columns[*pk], &columns[*fk]);
            if pk.ty != fk.ty {
                return Err(binder_error(format!(
                    "Failed to create foreign key: incompatible types between column {} (\"{}\") and column {} (\"{}\")",
                    pk.name, pk.ty, fk.name, fk.ty
                )));
            }
        }
        // keys compare in the order of the UNIQUE that they use
        let key = uniques
            .iter()
            .map(|(key, _)| key)
            .find(|key| key.len() == wanted.len() && key.iter().all(|c| wanted.contains(c)))
            .expect("find_referenced_key found it");
        let columns = key
            .iter()
            .map(|c| foreign[wanted.iter().position(|w| w == c).expect("the same columns")])
            .collect();
        Ok(ForeignKey {
            columns,
            referenced: parent.as_ref().map(|parent| ReferencedTable {
                schema: Arc::downgrade(schema),
                name: parent.name.clone(),
            }),
            referenced_columns: key.clone(),
        })
    }
}
