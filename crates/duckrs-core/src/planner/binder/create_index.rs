// Ported from the INDEX_ENTRY case of duckdb/src/planner/binder/statement/bind_create.cpp and
// duckdb/src/planner/expression_binder/index_binder.cpp @ rust-port-baseline
//
//! Binds CREATE INDEX.

use super::statements::schema_of;
use super::{Binder, Binding, binder_error};
use crate::catalog::{IndexCatalogEntry, TableCatalogEntry, TableOrView, missing_entry};
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use duckrs_common::types::PhysicalType;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{CreateInfo, CreateKind, Expr, ExprKind, OnCreateConflict, sql_quoted_identifier};
use duckrs_parser::transformer::visit_expr_mut;
use std::sync::Arc;

impl Binder<'_> {
    /// The table and the index to add to it; `None` for an IF NOT EXISTS that finds the index.
    pub fn bind_create_index(
        &mut self,
        info: &CreateInfo,
    ) -> Result<Option<(Arc<TableCatalogEntry>, IndexCatalogEntry)>> {
        let CreateKind::Index {
            table,
            unique,
            index_type,
            expressions,
            ..
        } = &info.kind
        else {
            unreachable!("checked by the caller");
        };
        if info.temporary {
            return Err(binder_error("Temporary indexes are not supported"));
        }
        if !index_type.eq_ignore_ascii_case("ART") {
            return Err(binder_error(format!("Unknown index type: {index_type}")));
        }
        let schema = self.catalog.schema_for(schema_of(&info.name)?)?;
        let table = match schema.get_entry(table) {
            Some(TableOrView::Table(table)) => table,
            Some(TableOrView::View(_)) => return Err(binder_error("can only create an index on a base table")),
            None => return Err(missing_entry("Table", table)),
        };
        let name = &info.name.name;
        if schema.get_index(name).is_some() {
            if info.on_conflict == OnCreateConflict::Ignore {
                return Ok(None);
            }
            return Err(Error::new(
                ErrorKind::Catalog,
                format!("Index with name {} already exists!", sql_quoted_identifier(name)),
            ));
        }
        let mut bound = Vec::with_capacity(expressions.len());
        let mut names = Vec::with_capacity(expressions.len());
        let mut columns = Vec::new();
        for expression in expressions {
            let key = self.bind_index_expression(&table, expression)?;
            key.clone().walk_mut(&mut |e| {
                if let BoundExprKind::Reference(column) = e.kind
                    && !columns.contains(&column)
                {
                    columns.push(column);
                }
            });
            names.push(match &expression.kind {
                ExprKind::ColumnRef(parts) => parts.last().expect("a column has a name").clone(),
                _ => expression.to_string(),
            });
            bound.push(key);
        }
        Ok(Some((
            table,
            IndexCatalogEntry {
                name: name.clone(),
                unique: *unique,
                expressions: bound,
                names,
                columns,
            },
        )))
    }

    /// `IndexBinder::BindExpression`.
    fn bind_index_expression(&self, table: &TableCatalogEntry, expression: &Expr) -> Result<BoundExpr> {
        let mut error = None;
        visit_expr_mut(&mut expression.clone(), &mut |e: &mut Expr| match &e.kind {
            ExprKind::Subquery { .. } => error = Some("cannot use subquery in index expressions"),
            ExprKind::Window(_) => error = Some("window functions are not allowed in index expressions"),
            ExprKind::ColumnRef(names)
                if names.last().is_some_and(|n| n.eq_ignore_ascii_case("rowid"))
                    && table.column_index("rowid").is_none() =>
            {
                error = Some("cannot create an index on the rowid");
            }
            _ => {}
        });
        if let Some(error) = error {
            return Err(binder_error(error));
        }
        let mut binder = Binder::new(self.catalog);
        binder.aggregate_error = "aggregate functions are not allowed in index expressions";
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
        if bound.is_volatile() {
            return Err(binder_error(
                "Index keys cannot contain expressions with side effects. They also cannot depend on settings such \
                 as TimeZone.",
            ));
        }
        if matches!(
            bound.return_type.physical_type(),
            PhysicalType::List | PhysicalType::Array | PhysicalType::Struct
        ) {
            return Err(Error::new(
                ErrorKind::InvalidType,
                format!("Invalid Type [{}]: Invalid type for index key.", bound.return_type),
            ));
        }
        Ok(bound)
    }
}
