// Ported from `Binder::GenerateMergeInto` and `DoUpdateSetQualify` of
// duckdb/src/planner/binder/statement/bind_insert.cpp @ rust-port-baseline
//
//! Binds ON CONFLICT and INSERT OR REPLACE. C++ turns them into a MERGE INTO, which this port
//! doesn't have yet; `execution::upsert` does what that MERGE comes down to.

use super::expression::checked_cast;
use super::{Binder, Binding, binder_error};
use crate::catalog::TableCatalogEntry;
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::{BoundExpr, ColumnBinding};
use crate::planner::logical::OnConflict;
use duckrs_common::Result;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_parser::ast::{Expr, ExprKind, InsertStatement, OnConflictAction, OnConflictInfo, sql_quoted_identifier};
use duckrs_parser::transformer::visit_expr_mut;
use std::collections::BTreeSet;

fn unique_keys(table: &TableCatalogEntry) -> Vec<Vec<usize>> {
    table.keys().into_iter().map(|key| key.columns).collect()
}

/// `DoUpdateSetQualify`: unqualified columns are the table's, not `excluded`'s.
fn qualify(expr: &Expr, table: &str) -> Result<Expr> {
    let mut lambda_parameters = BTreeSet::new();
    let mut expr = expr.clone();
    let mut subquery = false;
    visit_expr_mut(&mut expr, &mut |e: &mut Expr| match &mut e.kind {
        ExprKind::Subquery { .. } => subquery = true,
        ExprKind::Lambda { lhs, .. } => {
            visit_expr_mut(lhs, &mut |p: &mut Expr| {
                if let ExprKind::ColumnRef(names) = &p.kind {
                    lambda_parameters.extend(names.iter().map(|n| n.to_lowercase()));
                }
            });
        }
        ExprKind::ColumnRef(names) if names.len() == 1 && !lambda_parameters.contains(&names[0].to_lowercase()) => {
            names.insert(0, table.to_string());
        }
        _ => {}
    });
    if subquery {
        return Err(binder_error("DO UPDATE SET clause cannot contain a subquery"));
    }
    Ok(expr)
}

impl Binder<'_> {
    /// The checks of `GenerateMergeInto` and the SET clause.
    pub(super) fn bind_on_conflict(
        &mut self,
        insert: &InsertStatement,
        info: &OnConflictInfo,
        table: &TableCatalogEntry,
    ) -> Result<OnConflict> {
        let keys = unique_keys(table);
        let targets: Vec<Vec<usize>> = if info.indexed_columns.is_empty() {
            if keys.is_empty() {
                return Err(binder_error(
                    "There are no UNIQUE/PRIMARY KEY constraints that refer to this table, specify ON CONFLICT \
                     columns manually",
                ));
            }
            if keys.len() != 1 && info.action != OnConflictAction::Nothing {
                return Err(binder_error(
                    "Conflict target has to be provided for a DO UPDATE operation when the table has multiple \
                     UNIQUE/PRIMARY KEY constraints",
                ));
            }
            keys.clone()
        } else {
            let mut named = BTreeSet::new();
            for name in &info.indexed_columns {
                named.insert(self.conflict_column(table, name)?);
            }
            let key = keys
                .iter()
                .find(|key| key.iter().copied().collect::<BTreeSet<usize>>() == named);
            let Some(key) = key else {
                return Err(binder_error(
                    "The specified columns as conflict target are not referenced by a UNIQUE/PRIMARY KEY CONSTRAINT \
                     or INDEX",
                ));
            };
            vec![key.clone()]
        };
        if info.condition.is_some() {
            return Err(binder_error(
                "ON CONFLICT WHERE clause is only supported in DO UPDATE SET ... WHERE ...\nThe WHERE clause after \
                 the conflict columns is used for partial indexes which are not supported.",
            ));
        }
        let mut result = OnConflict {
            do_update: info.action != OnConflictAction::Nothing,
            targets,
            set_columns: Vec::new(),
            set_expressions: Vec::new(),
            condition: None,
        };
        // the columns of the row and of `excluded` come one after the other
        let mut binder = Binder::new(self.catalog);
        binder.aggregate_error = "DO UPDATE SET clause cannot contain aggregates!";
        let alias = insert.table_alias.clone().unwrap_or_else(|| table.name.clone());
        let row_index = binder.generate_table_index();
        let excluded_index = binder.generate_table_index();
        for (name, table_index) in [(alias.clone(), row_index), ("excluded".to_string(), excluded_index)] {
            binder.bind_context.add_binding(Binding {
                alias: name,
                schema: None,
                table_index,
                names: table.columns.iter().map(|c| c.name.clone()).collect(),
                types: table.types(),
                row_id: None,
            });
        }
        let bindings: Vec<ColumnBinding> = [row_index, excluded_index]
            .into_iter()
            .flat_map(|table_index| {
                (0..table.columns.len()).map(move |column_index| ColumnBinding {
                    table_index,
                    column_index,
                })
            })
            .collect();
        let resolved = |mut bound: BoundExpr| -> Result<BoundExpr> {
            resolve_expression(&mut bound, &bindings)?;
            Ok(bound)
        };
        match info.action {
            OnConflictAction::Nothing => {}
            OnConflictAction::Replace => {
                // `CreateSetInfoForReplace`: what isn't a key takes the new value
                let is_key = |column: usize| keys.iter().any(|key| key.contains(&column));
                let columns: Vec<usize> = if insert.columns.is_empty() {
                    (0..table.columns.len()).collect()
                } else {
                    insert
                        .columns
                        .iter()
                        .filter_map(|name| table.column_index(name))
                        .collect()
                };
                for column in columns.into_iter().filter(|c| !is_key(*c)) {
                    let excluded = Expr::column(vec!["excluded".into(), table.columns[column].name.clone()]);
                    result.set_columns.push(column);
                    result
                        .set_expressions
                        .push(resolved(binder.bind_expression(&excluded)?)?);
                }
            }
            OnConflictAction::Update => {
                let set_info = info
                    .set_info
                    .as_ref()
                    .ok_or_else(|| crate::unported("DO UPDATE without SET"))?;
                for (name, expression) in set_info.columns.iter().zip(&set_info.expressions) {
                    let column = self.conflict_column(table, name)?;
                    if result.set_columns.contains(&column) {
                        return Err(binder_error("Multiple assignments to same column"));
                    }
                    let target = &table.columns[column];
                    let bound = if matches!(expression.kind, ExprKind::Default) {
                        target
                            .default_value
                            .clone()
                            .unwrap_or_else(|| BoundExpr::constant(Value::null(target.ty.clone())))
                    } else {
                        let bound = binder.bind_expression(&qualify(expression, &alias)?)?;
                        checked_cast(bound, &target.ty)?
                    };
                    result.set_columns.push(column);
                    result.set_expressions.push(resolved(bound)?);
                }
                if let Some(condition) = &set_info.condition {
                    let mut has_default = false;
                    visit_expr_mut(&mut condition.as_ref().clone(), &mut |e: &mut Expr| {
                        has_default |= matches!(e.kind, ExprKind::Default);
                    });
                    if has_default {
                        return Err(binder_error("WHERE clause cannot contain DEFAULT clause"));
                    }
                    let bound = binder.bind_expression(&qualify(condition, &alias)?)?;
                    let bound = checked_cast(bound, &LogicalType::BOOLEAN)?;
                    result.condition = Some(resolved(bound)?);
                }
            }
        }
        Ok(result)
    }

    fn conflict_column(&self, table: &TableCatalogEntry, name: &str) -> Result<usize> {
        table.column_index(name).ok_or_else(|| {
            binder_error(format!(
                "Table {} does not have a column named {}",
                sql_quoted_identifier(&table.name),
                sql_quoted_identifier(name)
            ))
        })
    }
}
