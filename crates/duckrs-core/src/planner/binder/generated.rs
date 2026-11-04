// Ported from duckdb/src/planner/binder/statement/bind_create_table.cpp (BindGeneratedColumns),
// duckdb/src/catalog/catalog_entry/column_dependency_manager.cpp and
// duckdb/src/parser/column_definition.cpp @ rust-port-baseline
//
//! Generated columns. They are not stored: a reference to one binds its expression instead.

use super::{Binder, Binding, binder_error};
use crate::catalog::{GeneratedColumn, TableCatalogEntry};
use crate::function::binder::expression_return_type;
use crate::function::combine_types::normalize_type;
use crate::planner::column_binding_resolver::resolve_expression;
use crate::planner::expression::ColumnBinding;
use duckrs_common::types::LogicalType;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{self, ColumnDefinition as ParsedColumn, Expr, ExprKind};
use duckrs_parser::transformer::visit_expr_mut;

/// The columns an expression reads (`ColumnDefinition::GetListOfDependencies`).
pub(crate) fn dependencies(expr: &Expr) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut lambda = false;
    let mut qualified = false;
    visit_expr_mut(&mut expr.clone(), &mut |e| match &e.kind {
        ExprKind::ColumnRef(columns) => {
            qualified |= columns.len() > 1;
            names.push(columns.last().expect("a name").clone());
        }
        ExprKind::Lambda { .. } => lambda = true,
        _ => {}
    });
    if lambda {
        return Err(crate::error_not_implemented(
            "Lambda functions are currently not supported in generated columns.",
        ));
    }
    if qualified {
        return Err(binder_error(
            "Qualified (tbl.name) column references are not allowed inside of generated column expressions",
        ));
    }
    Ok(names)
}

/// Starts the name of a column that is referred to by its position in the table.
pub(super) const POSITIONAL_COLUMN: &str = "\u{1}#";

/// Column names of `expr` as `alias.name`, so that they find their table in any query.
pub(super) fn qualify(expr: &Expr, alias: &str) -> Expr {
    qualify_renamed(expr, alias, &[], &[])
}

/// [`qualify`] for a table whose columns `original` are called `renamed` in the query.
fn qualify_renamed(expr: &Expr, alias: &str, original: &[&str], renamed: &[String]) -> Expr {
    let mut expr = expr.clone();
    visit_expr_mut(&mut expr, &mut |e| {
        if let ExprKind::ColumnRef(names) = &mut e.kind {
            let column = names.last().expect("a name").clone();
            // by position: renaming can leave two columns with one name
            let column = match original.iter().position(|c| c.eq_ignore_ascii_case(&column)) {
                Some(index) if !renamed.is_empty() => format!("{POSITIONAL_COLUMN}{index}"),
                _ => column,
            };
            *names = vec![alias.to_string(), column];
        }
    });
    expr
}

/// A binding of the columns of a table, generated ones included.
pub(super) fn table_binding(
    table: &TableCatalogEntry,
    alias: String,
    table_index: usize,
    names: Vec<String>,
) -> Binding {
    let columns = table.logical_columns();
    let generated = if table.generated.is_empty() {
        Vec::new()
    } else {
        let original: Vec<&str> = columns.iter().map(|c| c.name).collect();
        columns
            .iter()
            .map(|c| c.generated.map(|e| qualify_renamed(e, &alias, &original, &names)))
            .collect()
    };
    Binding {
        schema: None,
        types: columns.iter().map(|c| c.ty.clone()).collect(),
        alias,
        table_index,
        names,
        row_id: Some(table.columns.len()),
        generated,
    }
}

impl Binder<'_> {
    /// `BindGeneratedColumns`: for `parsed`, a whole column list; each column is bound after what it reads.
    pub(super) fn bind_generated_columns(
        &self,
        table: &str,
        parsed: &[ParsedColumn],
        physical: &[crate::catalog::ColumnDefinition],
    ) -> Result<Vec<GeneratedColumn>> {
        let mut names = Vec::with_capacity(parsed.len());
        let mut types: Vec<Option<LogicalType>> = Vec::with_capacity(parsed.len());
        let mut reads: Vec<Vec<String>> = Vec::with_capacity(parsed.len());
        for column in parsed {
            names.push(column.name.clone());
            match &column.generated {
                Some(expr) => {
                    reads.push(dependencies(expr)?);
                    types.push(column.column_type.as_ref().map(|t| self.bind_type(t)).transpose()?);
                }
                None => {
                    let definition = physical.iter().find(|c| c.name.eq_ignore_ascii_case(&column.name));
                    types.push(definition.map(|c| c.ty.clone()));
                    reads.push(Vec::new());
                }
            }
        }
        // `ColumnDependencyManager::GetBindOrder`
        let mut order = Vec::new();
        let mut state = vec![0u8; parsed.len()];
        fn visit(
            index: usize,
            names: &[String],
            reads: &[Vec<String>],
            state: &mut [u8],
            order: &mut Vec<usize>,
        ) -> Result<()> {
            match state[index] {
                2 => return Ok(()),
                1 => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "Circular dependency encountered when resolving generated column expressions",
                    ));
                }
                _ => {}
            }
            state[index] = 1;
            for name in &reads[index] {
                if let Some(next) = names.iter().position(|n| n.eq_ignore_ascii_case(name)) {
                    visit(next, names, reads, state, order)?;
                }
            }
            state[index] = 2;
            order.push(index);
            Ok(())
        }
        for index in 0..parsed.len() {
            visit(index, &names, &reads, &mut state, &mut order)?;
        }
        let mut result: Vec<Option<GeneratedColumn>> = vec![None; parsed.len()];
        for index in order {
            let Some(expr) = &parsed[index].generated else {
                continue;
            };
            if parsed[index].compression.is_some() {
                return Err(binder_error(format!(
                    "Can't set a compression type on generated column {}, generated columns are not stored",
                    parsed[index].name
                )));
            }
            let mut binder = Binder::new(self.catalog);
            binder.aggregate_error = "Aggregate functions are not allowed in generated columns";
            let table_index = binder.generate_table_index();
            let usable = |i: usize| types[i].clone().unwrap_or(LogicalType::SQLNULL);
            binder.bind_context.add_binding(Binding {
                alias: table.to_string(),
                schema: None,
                table_index,
                names: names.clone(),
                types: (0..names.len()).map(usable).collect(),
                row_id: None,
                generated: parsed
                    .iter()
                    .map(|c| c.generated.as_deref().map(|e| qualify(e, table)))
                    .collect(),
            });
            let mut bound = binder.bind_expression(&qualify(expr, table))?;
            if types[index].is_none() {
                types[index] = Some(normalize_type(&expression_return_type(&bound)));
            }
            let stored = parsed.iter().filter(|c| c.generated.is_none()).count();
            let bindings: Vec<ColumnBinding> = (0..stored)
                .map(|column_index| ColumnBinding {
                    table_index,
                    column_index,
                })
                .collect();
            resolve_expression(&mut bound, &bindings)?;
            result[index] = Some(GeneratedColumn {
                name: parsed[index].name.clone(),
                ty: types[index].clone().expect("set above"),
                expr: (**expr).clone(),
                bound,
                position: index,
            });
        }
        Ok(result.into_iter().flatten().collect())
    }
}

/// `AnyConstraintReferencesGeneratedColumn`, and the constraints with the indexes of stored columns.
pub(super) fn physical_constraints(info: &ast::CreateTableInfo) -> Result<Vec<ast::Constraint>> {
    const ERROR: &str = "Constraints on generated columns are not supported yet";
    let generated = |name: &str| {
        info.columns
            .iter()
            .any(|c| c.generated.is_some() && c.name.eq_ignore_ascii_case(name))
    };
    let physical_index = |index: usize| -> Result<usize> {
        if info.columns[index].generated.is_some() {
            return Err(binder_error(ERROR));
        }
        Ok(info.columns[..index].iter().filter(|c| c.generated.is_none()).count())
    };
    let mut result = Vec::with_capacity(info.constraints.len());
    for constraint in &info.constraints {
        let mut constraint = constraint.clone();
        match &mut constraint {
            ast::Constraint::NotNull(index) => *index = physical_index(*index)?,
            ast::Constraint::Unique { index, columns, .. } => {
                if let Some(index) = index {
                    *index = physical_index(*index)?;
                }
                if columns.iter().any(|c| generated(c)) {
                    return Err(binder_error(ERROR));
                }
            }
            ast::Constraint::Check(expr) => {
                let mut found = false;
                visit_expr_mut(expr, &mut |e| {
                    if let ExprKind::ColumnRef(names) = &e.kind {
                        found |= generated(names.last().expect("a name"));
                    }
                });
                if found {
                    return Err(binder_error(ERROR));
                }
            }
            ast::Constraint::ForeignKey { fk_columns, .. } => {
                if let Some(column) = fk_columns.iter().find(|c| generated(c)) {
                    return Err(binder_error(format!(
                        "Failed to create foreign key: referenced column \"{column}\" is a generated column"
                    )));
                }
            }
        }
        result.push(constraint);
    }
    Ok(result)
}
