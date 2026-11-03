// Ported from duckdb/src/planner/binder/query_node/{bind_select_node,plan_select_node}.cpp,
// duckdb/src/planner/binder/tableref/{bind_basetableref,bind_expressionlistref,
// bind_emptytableref}.cpp, duckdb/src/planner/binder/statement/{bind_select,bind_insert,
// bind_create_table}.cpp and duckdb/src/planner/bind_context.cpp (alias and star expansion)
// @ rust-port-baseline
//
//! Binding SELECT (one table or VALUES, WHERE, a select list), INSERT and CREATE TABLE.

use super::{Binder, Binding, BoundStatement, StatementReturnType, binder_error};
use crate::catalog::{ColumnDefinition, DEFAULT_CATALOG, OnCreateConflict};
use crate::function::binder::expression_return_type;
use crate::function::combine_types::{normalize_type, try_max_logical_type};
use crate::planner::expression::BoundExpr;
use crate::planner::logical::{LogicalOperator, LogicalOperatorKind};
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    CreateTableInfo, Expr, ExprKind, InsertStatement, InsertValues, OnCreateConflict as AstConflict, QualifiedName,
    QueryNodeKind, SelectNode, SelectStatement, TableRef, TableRefKind, values_list,
};

fn not_implemented(what: &str) -> Error {
    crate::unported(what)
}

/// `ParsedExpression::GetName`: the alias, a column's own name, or the expression's SQL.
fn expression_name(expr: &Expr) -> String {
    if !expr.alias.is_empty() {
        return expr.alias.clone();
    }
    match &expr.kind {
        ExprKind::ColumnRef(names) => names.last().cloned().unwrap_or_default(),
        _ => expr.to_string(),
    }
}

/// The schema of a qualified table name, checking that its catalog is ours.
fn schema_of(name: &QualifiedName) -> Result<Option<&str>> {
    match name.qualification.as_slice() {
        [] => Ok(None),
        [schema] => Ok(Some(schema)),
        [catalog, schema] => {
            if !catalog.eq_ignore_ascii_case(DEFAULT_CATALOG) {
                return Err(binder_error(format!("Catalog \"{catalog}\" does not exist!")));
            }
            Ok(Some(schema))
        }
        _ => Err(not_implemented("a nested schema path")),
    }
}

impl Binder<'_> {
    /// `Binder::Bind(SelectStatement)`.
    pub(super) fn bind_select_statement(&mut self, select: &SelectStatement) -> Result<BoundStatement> {
        let node = &select.node;
        if !node.modifiers.is_empty() {
            return Err(not_implemented("ORDER BY, LIMIT and DISTINCT"));
        }
        if !node.cte_map.0.is_empty() {
            return Err(not_implemented("WITH"));
        }
        match &node.kind {
            QueryNodeKind::Select(select) => self.bind_select_node(select),
            QueryNodeKind::SetOperation(_) => Err(not_implemented("UNION, EXCEPT and INTERSECT")),
            _ => Err(not_implemented("this kind of query")),
        }
    }

    /// `Binder::BindNode(SelectNode)` + `CreatePlan(BoundSelectNode)`.
    fn bind_select_node(&mut self, node: &SelectNode) -> Result<BoundStatement> {
        if !node.groups.group_expressions.is_empty() || node.force_aggregates || node.having.is_some() {
            return Err(not_implemented("GROUP BY"));
        }
        if node.qualify.is_some() {
            return Err(not_implemented("QUALIFY"));
        }
        if node.sample.is_some() {
            return Err(not_implemented("SAMPLE"));
        }
        let mut plan = self.bind_table_ref(&node.from_table)?;
        if let Some(condition) = &node.where_clause {
            let condition = self.bind_expression(condition)?.cast_to(&LogicalType::BOOLEAN, false);
            plan = LogicalOperator::new(
                LogicalOperatorKind::Filter {
                    expressions: vec![condition],
                },
                vec![plan],
            );
        }
        let select_list = self.expand_star_expressions(&node.select_list)?;
        let mut names = Vec::with_capacity(select_list.len());
        let mut expressions = Vec::with_capacity(select_list.len());
        for expr in &select_list {
            names.push(expression_name(expr));
            expressions.push(self.bind_expression(expr)?);
        }
        let types = expressions.iter().map(|e: &BoundExpr| e.return_type.clone()).collect();
        let table_index = self.generate_table_index();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index,
                expressions,
            },
            vec![plan],
        );
        Ok(BoundStatement {
            plan,
            names,
            types,
            return_type: StatementReturnType::QueryResult,
        })
    }

    /// `BindContext::GenerateAllColumnExpressions`: replaces `*` and `t.*` with the columns of
    /// the bindings they stand for.
    fn expand_star_expressions(&self, select_list: &[Expr]) -> Result<Vec<Expr>> {
        let mut result = Vec::new();
        for expr in select_list {
            let ExprKind::Star(star) = &expr.kind else {
                result.push(expr.clone());
                continue;
            };
            if star.columns
                || star.expr.is_some()
                || !star.exclude_list.is_empty()
                || !star.replace_list.is_empty()
                || !star.rename_list.is_empty()
            {
                return Err(not_implemented("COLUMNS, EXCLUDE, REPLACE and RENAME"));
            }
            if self.bind_context.bindings.is_empty() {
                return Err(binder_error("SELECT * expression without FROM clause!"));
            }
            let bindings: Vec<&Binding> = if star.relation_name.is_empty() {
                self.bind_context.bindings.iter().collect()
            } else {
                let found: Vec<&Binding> = self
                    .bind_context
                    .bindings
                    .iter()
                    .filter(|b| b.alias.eq_ignore_ascii_case(&star.relation_name))
                    .collect();
                if found.is_empty() {
                    return Err(binder_error(format!(
                        "Referenced table \"{}\" not found!",
                        star.relation_name
                    )));
                }
                found
            };
            for binding in bindings {
                for name in &binding.names {
                    let mut column: Expr = ExprKind::ColumnRef(vec![binding.alias.clone(), name.clone()]).into();
                    column.alias = name.clone();
                    result.push(column);
                }
            }
        }
        Ok(result)
    }

    /// Applies a table reference's column aliases (`t(a, b)`) to its column names.
    fn alias_columns(table_ref: &TableRef, alias: &str, mut names: Vec<String>) -> Result<Vec<String>> {
        if table_ref.column_name_alias.len() > names.len() {
            return Err(binder_error(format!(
                "table {alias} has {} columns available but {} columns specified",
                names.len(),
                table_ref.column_name_alias.len()
            )));
        }
        for (name, alias) in names.iter_mut().zip(&table_ref.column_name_alias) {
            *name = alias.clone();
        }
        Ok(names)
    }

    /// `Binder::Bind(TableRef)` + `CreatePlan`: the FROM clause.
    fn bind_table_ref(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
        if table_ref.sample.is_some() {
            return Err(not_implemented("TABLESAMPLE"));
        }
        match &table_ref.kind {
            TableRefKind::Empty => {
                let table_index = self.generate_table_index();
                Ok(LogicalOperator::new(
                    LogicalOperatorKind::DummyScan { table_index },
                    vec![],
                ))
            }
            TableRefKind::BaseTable { name, at_clause } => {
                if at_clause.is_some() {
                    return Err(not_implemented("AT clauses"));
                }
                let table = self.catalog.get_table(schema_of(name)?, &name.name)?;
                let alias = if table_ref.alias.is_empty() {
                    table.name.clone()
                } else {
                    table_ref.alias.clone()
                };
                let names = table.columns.iter().map(|c| c.name.clone()).collect();
                let names = Self::alias_columns(table_ref, &alias, names)?;
                let table_index = self.generate_table_index();
                self.bind_context.add_binding(Binding {
                    alias,
                    table_index,
                    names,
                    types: table.types(),
                })?;
                let column_ids = (0..table.columns.len()).collect();
                Ok(LogicalOperator::new(
                    LogicalOperatorKind::Get {
                        table,
                        table_index,
                        column_ids,
                    },
                    vec![],
                ))
            }
            TableRefKind::ExpressionList(rows) => {
                let (types, expressions) = self.bind_values(rows, None)?;
                let alias = if table_ref.alias.is_empty() {
                    "valueslist".to_string()
                } else {
                    table_ref.alias.clone()
                };
                let names = (0..types.len()).map(|i| format!("col{i}")).collect();
                let names = Self::alias_columns(table_ref, &alias, names)?;
                let table_index = self.generate_table_index();
                self.bind_context.add_binding(Binding {
                    alias,
                    table_index,
                    names,
                    types: types.clone(),
                })?;
                Ok(LogicalOperator::new(
                    LogicalOperatorKind::ExpressionGet {
                        table_index,
                        types,
                        expressions,
                    },
                    vec![],
                ))
            }
            TableRefKind::Join(_) => Err(not_implemented("joins")),
            TableRefKind::Subquery(_) => Err(not_implemented("subqueries in FROM")),
            TableRefKind::TableFunction { .. } => Err(not_implemented("table functions")),
            _ => Err(not_implemented("this kind of table reference")),
        }
    }

    /// `Binder::Bind(ExpressionListRef)`: binds the rows of a VALUES list. With
    /// `expected_types`, every value is cast to its column's type; without, each column gets
    /// the type all its values can be cast to.
    fn bind_values(
        &mut self,
        rows: &[Vec<Expr>],
        expected_types: Option<&[LogicalType]>,
    ) -> Result<(Vec<LogicalType>, Vec<Vec<BoundExpr>>)> {
        let column_count = rows.first().map_or(0, Vec::len);
        if rows.iter().any(|row| row.len() != column_count) {
            return Err(binder_error("VALUES lists must all be the same length"));
        }
        let mut bound = Vec::with_capacity(rows.len());
        for row in rows {
            let mut list = Vec::with_capacity(row.len());
            for (i, expr) in row.iter().enumerate() {
                let value = match (&expr.kind, expected_types) {
                    // DEFAULT is the column's default, which is NULL until defaults are supported
                    (ExprKind::Default, Some(types)) => BoundExpr::constant(Value::null(types[i].clone())),
                    (ExprKind::Default, None) => {
                        return Err(binder_error("DEFAULT is not allowed here!"));
                    }
                    _ => self.bind_expression(expr)?,
                };
                list.push(value);
            }
            bound.push(list);
        }
        let types = match expected_types {
            Some(types) => types.to_vec(),
            None => {
                let mut types = vec![LogicalType::SQLNULL; column_count];
                for list in &bound {
                    for (ty, value) in types.iter_mut().zip(list) {
                        let next = expression_return_type(value);
                        *ty = try_max_logical_type(ty, &next).ok_or_else(|| {
                            Error::new(
                                ErrorKind::NotImplemented,
                                format!("Cannot combine types {ty} and {next} - an explicit cast is required"),
                            )
                        })?;
                    }
                }
                types.iter().map(normalize_type).collect()
            }
        };
        for list in &mut bound {
            for (value, ty) in list.iter_mut().zip(&types) {
                check_cast(&value.return_type, ty)?;
                *value = std::mem::replace(value, BoundExpr::constant(Value::sql_null())).cast_to(ty, false);
            }
        }
        Ok((types, bound))
    }

    /// `Binder::Bind(InsertStatement)`.
    pub(super) fn bind_insert(&mut self, insert: &InsertStatement) -> Result<BoundStatement> {
        if !insert.cte_map.0.is_empty() {
            return Err(not_implemented("WITH in INSERT"));
        }
        if insert.on_conflict.is_some() {
            return Err(not_implemented("ON CONFLICT"));
        }
        if !insert.returning.is_empty() {
            return Err(not_implemented("RETURNING"));
        }
        if insert.by_name {
            return Err(not_implemented("INSERT BY NAME"));
        }
        let table = self.catalog.get_table(schema_of(&insert.table)?, &insert.table.name)?;
        // BindInsertColumnList: which table column each inserted column fills
        let mut expected_types = Vec::new();
        let mut column_index_map: Vec<Option<usize>>;
        if insert.columns.is_empty() {
            expected_types = table.types();
            column_index_map = (0..table.columns.len()).map(Some).collect();
        } else {
            let mut named = Vec::new();
            for (i, name) in insert.columns.iter().enumerate() {
                if insert.columns[..i].iter().any(|n| n.eq_ignore_ascii_case(name)) {
                    return Err(binder_error(format!("Duplicate column name {name} in INSERT")));
                }
                let Some(index) = table.column_index(name) else {
                    return Err(binder_error(format!(
                        "Table {} does not have a column with name {name}",
                        table.name
                    )));
                };
                expected_types.push(table.columns[index].ty.clone());
                named.push(index);
            }
            column_index_map = vec![None; table.columns.len()];
            for (position, &index) in named.iter().enumerate() {
                column_index_map[index] = Some(position);
            }
        }
        let columns_provided = !insert.columns.is_empty();
        let mismatch = |expected: usize, supplied: usize| {
            binder_error(if columns_provided {
                format!(
                    "Column name/value mismatch for insert on {}: expected {expected} columns but {supplied} values \
                     were supplied",
                    table.name
                )
            } else {
                format!(
                    "table {} has {expected} columns but {supplied} values were supplied",
                    table.name
                )
            })
        };
        let source = match &insert.values {
            InsertValues::DefaultValues => {
                // a single row of defaults
                column_index_map = vec![None; table.columns.len()];
                let table_index = self.generate_table_index();
                LogicalOperator::new(LogicalOperatorKind::DummyScan { table_index }, vec![])
            }
            InsertValues::Select(select) => match values_list(select) {
                Some(rows) => {
                    if let Some(row) = rows.first()
                        && row.len() != expected_types.len()
                    {
                        return Err(mismatch(expected_types.len(), row.len()));
                    }
                    let (types, expressions) = self.bind_values(rows, Some(&expected_types))?;
                    let table_index = self.generate_table_index();
                    LogicalOperator::new(
                        LogicalOperatorKind::ExpressionGet {
                            table_index,
                            types,
                            expressions,
                        },
                        vec![],
                    )
                }
                None => {
                    let mut binder = Binder::new(self.catalog);
                    binder.next_table_index = self.next_table_index;
                    let bound = binder.bind_select_statement(select)?;
                    self.next_table_index = binder.next_table_index;
                    if bound.types.len() != expected_types.len() {
                        return Err(mismatch(expected_types.len(), bound.types.len()));
                    }
                    // cast the query's columns to the column types
                    let mut plan = bound.plan;
                    if let LogicalOperatorKind::Projection { expressions, .. } = &mut plan.kind {
                        for (expr, ty) in expressions.iter_mut().zip(&expected_types) {
                            check_cast(&expr.return_type, ty)?;
                            *expr = std::mem::replace(expr, BoundExpr::constant(Value::sql_null())).cast_to(ty, false);
                        }
                    }
                    plan
                }
            },
        };
        let plan = LogicalOperator::new(
            LogicalOperatorKind::Insert {
                table,
                column_index_map,
            },
            vec![source],
        );
        Ok(BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            return_type: StatementReturnType::ChangedRows,
        })
    }

    /// `Binder::BindCreateTableInfo`.
    pub(super) fn bind_create_table(&mut self, info: &CreateTableInfo) -> Result<BoundStatement> {
        if info.temporary {
            return Err(not_implemented("CREATE TEMPORARY TABLE"));
        }
        if info.query.is_some() {
            return Err(not_implemented("CREATE TABLE AS"));
        }
        if !info.constraints.is_empty() {
            return Err(not_implemented("table constraints"));
        }
        if !info.partition_keys.is_empty() || !info.sort_keys.is_empty() || !info.options.is_empty() {
            return Err(not_implemented("table options"));
        }
        let mut columns = Vec::with_capacity(info.columns.len());
        for column in &info.columns {
            if column.generated.is_some() {
                return Err(not_implemented("generated columns"));
            }
            if column.default_value.is_some() {
                return Err(not_implemented("column defaults"));
            }
            if columns
                .iter()
                .any(|c: &ColumnDefinition| c.name.eq_ignore_ascii_case(&column.name))
            {
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!("Column with name {} already exists!", column.name),
                ));
            }
            let Some(ty) = &column.column_type else {
                return Err(not_implemented("columns without a type"));
            };
            columns.push(ColumnDefinition {
                name: column.name.clone(),
                ty: self.bind_type(ty)?,
            });
        }
        let schema = self.catalog.schema_for(schema_of(&info.name)?)?;
        let on_conflict = match info.on_conflict {
            AstConflict::Error => OnCreateConflict::Error,
            AstConflict::Ignore => OnCreateConflict::Ignore,
            AstConflict::Replace => OnCreateConflict::Replace,
        };
        let plan = LogicalOperator::new(
            LogicalOperatorKind::CreateTable {
                schema,
                name: info.name.name.clone(),
                columns,
                on_conflict,
            },
            vec![],
        );
        Ok(BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            return_type: StatementReturnType::Nothing,
        })
    }
}

/// Fails like DuckDB would for a cast that is not implemented here yet.
fn check_cast(from: &LogicalType, to: &LogicalType) -> Result<()> {
    if crate::function::cast::has_cast(from, to) {
        Ok(())
    } else {
        Err(crate::unported(format!("casting {from} to {to}")))
    }
}
