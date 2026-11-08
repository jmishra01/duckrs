// Ported from the INDEX_ENTRY case of duckdb/src/planner/binder/statement/bind_create.cpp and
// duckdb/src/planner/expression_binder/index_binder.cpp @ rust-port-baseline
//
//! Binds CREATE INDEX.

use super::{Binder, Binding, binder_error};
use crate::catalog::{IndexCatalogEntry, TableCatalogEntry, TableOrView, missing_entry};
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use duckrs_common::types::{LogicalType, LogicalTypeId, PhysicalType};
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    CreateInfo, CreateKind, Expr, ExprKind, OnCreateConflict, sql_identifier, sql_quoted_identifier,
};
use duckrs_parser::transformer::visit_expr_mut;
use std::sync::Arc;

const SIDE_EFFECTS: &str =
    "Index keys cannot contain expressions with side effects. They also cannot depend on settings such as TimeZone.";

/// What depends on the session or the query, which a key can't: the next one would differ.
const SESSION_FUNCTIONS: [&str; 19] = [
    "now",
    "today",
    "get_current_time",
    "get_current_timestamp",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_localtime",
    "current_localtimestamp",
    "transaction_timestamp",
    "txid_current",
    "current_transaction_id",
    "getvariable",
    "current_setting",
    "current_schema",
    "current_schemas",
    "current_database",
    "current_catalog",
    "current_query",
];

/// A type as C++ prints it, which quotes the members of a UNION that are keywords.
fn type_name(ty: &LogicalType) -> String {
    if ty.id() != LogicalTypeId::Union {
        return ty.to_string();
    }
    let members = ty.struct_children().iter().skip(1);
    let members: Vec<String> = members
        .map(|(name, ty)| format!("{} {ty}", sql_identifier(name)))
        .collect();
    format!("UNION({})", members.join(", "))
}

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
        let schema = self
            .catalog
            .schema_with_entry(self.schema_of(&info.name)?.as_deref(), table)?;
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
            let (key, display) = self.bind_index_expression(&table, expression)?;
            // lambda functions need a ClientContext to execute, which appends and deletes do not have
            let mut lambda = None;
            key.clone().walk_mut(&mut |e| {
                if let BoundExprKind::ListLambda { kind, .. } = &e.kind {
                    lambda.get_or_insert(match kind {
                        crate::planner::expression::LambdaKind::Transform => "list_transform",
                        crate::planner::expression::LambdaKind::Filter => "list_filter",
                        crate::planner::expression::LambdaKind::Reduce => "list_reduce",
                    });
                }
            });
            if let Some(name) = lambda {
                return Err(binder_error(format!(
                    "Index keys cannot contain lambda functions such as \"{name}\"."
                )));
            }
            key.clone().walk_mut(&mut |e| {
                if let BoundExprKind::Reference(column) = e.kind
                    && !columns.contains(&column)
                {
                    columns.push(column);
                }
            });
            names.push(match &expression.kind {
                ExprKind::ColumnRef(parts) => parts.last().expect("a column has a name").clone(),
                _ if self.calls_macro(&table, expression) => display,
                _ => expression.to_string(),
            });
            bound.push(key);
        }
        if columns.is_empty() {
            return Err(binder_error(
                "CREATE INDEX does not refer to any columns in the base table!",
            ));
        }
        Ok(Some((
            table,
            IndexCatalogEntry {
                name: name.clone(),
                unique: *unique,
                expressions: bound,
                names,
                columns,
                parsed: expressions.clone(),
                index_type: index_type.clone(),
                sql: info.to_string(),
                comment: None,
            },
        )))
    }

    /// `IndexBinder::BindExpression`.
    fn bind_index_expression(&self, table: &TableCatalogEntry, expression: &Expr) -> Result<(BoundExpr, String)> {
        let mut error = None;
        visit_expr_mut(&mut expression.clone(), &mut |e: &mut Expr| match &e.kind {
            ExprKind::Function(function) if SESSION_FUNCTIONS.contains(&function.name.name.to_lowercase().as_str()) => {
                error = Some(SIDE_EFFECTS);
            }
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
            generated: Vec::new(),
        });
        let mut bound = binder.bind_expression(expression)?;
        let display = bound.to_string();
        let bindings: Vec<ColumnBinding> = (0..table.columns.len())
            .map(|column| ColumnBinding::new(table_index, column))
            .collect();
        resolve_expression(&mut bound, &bindings)?;
        if bound.is_volatile() {
            return Err(binder_error(SIDE_EFFECTS));
        }
        if matches!(
            bound.return_type.physical_type(),
            PhysicalType::List | PhysicalType::Array | PhysicalType::Struct | PhysicalType::Interval
        ) {
            return Err(Error::new(
                ErrorKind::InvalidType,
                format!(
                    "Invalid Type [{}]: Invalid type for index key.",
                    type_name(&bound.return_type)
                ),
            ));
        }
        Ok((bound, display))
    }

    /// Whether what a key was written as isn't what it is: C++ names the expression after expanding.
    fn calls_macro(&self, table: &TableCatalogEntry, expression: &Expr) -> bool {
        let schema = self.catalog.get_schema(&table.schema);
        let mut found = false;
        visit_expr_mut(&mut expression.clone(), &mut |e: &mut Expr| {
            if let ExprKind::Function(function) = &e.kind {
                found |= schema
                    .as_ref()
                    .is_some_and(|schema| schema.get_macro(&function.name.name, false).is_some());
            }
        });
        found
    }
}
