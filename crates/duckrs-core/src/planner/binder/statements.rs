// Ported from duckdb/src/planner/binder/query_node/{bind_select_node,plan_select_node}.cpp,
// duckdb/src/planner/binder/tableref/{bind_basetableref,bind_expressionlistref,
// bind_emptytableref}.cpp, duckdb/src/planner/binder/statement/{bind_select,bind_insert,
// bind_create_table}.cpp and duckdb/src/planner/bind_context.cpp (alias and star expansion)
// @ rust-port-baseline
//
//! Binding SELECT (one table or VALUES, WHERE, a select list, ORDER BY, LIMIT/OFFSET and
//! DISTINCT), INSERT and CREATE TABLE.

use super::{Binder, Binding, BoundStatement, StatementReturnType, binder_error};
use crate::catalog::{ColumnDefinition, DEFAULT_CATALOG, OnCreateConflict};
use crate::execution::expression_executor::evaluate_scalar;
use crate::function::binder::expression_return_type;
use crate::function::combine_types::{normalize_type, try_max_logical_type};
use crate::planner::expression::{BoundExpr, ColumnBinding};
use crate::planner::logical::{BoundOrderByNode, LogicalOperator, LogicalOperatorKind};
use duckrs_common::sort_key::OrderModifiers;
use duckrs_common::types::LogicalType;
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::{
    CreateTableInfo, Expr, ExprKind, InsertStatement, InsertValues, LiteralKind, OnCreateConflict as AstConflict,
    OrderByNode, QualifiedName, QueryNodeKind, ResultModifier, SelectNode, SelectStatement, TableRef, TableRefKind,
    values_list,
};
use duckrs_parser::transformer::visit_expr_mut;

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

/// An expression with all aliases removed: `ParsedExpression::Equals` ignores aliases.
fn without_aliases(expr: &Expr) -> Expr {
    let mut expr = expr.clone();
    visit_expr_mut(&mut expr, &mut |e| e.alias.clear());
    expr
}

/// What binding the select list needs to know about it (`SelectBindState`).
#[derive(Default)]
struct SelectBindState {
    /// alias -> select list index
    alias_map: Vec<(String, usize)>,
    /// (qualified, alias-free) select list entry -> index; the last one wins
    projection_map: Vec<(Expr, usize)>,
    /// the select list as written (qualified)
    original_expressions: Vec<Expr>,
}

/// A result modifier with its targets resolved to select list indexes.
enum PreparedModifier {
    Distinct,
    Order(Vec<(OrderModifiers, usize)>),
    OrderAll(OrderModifiers),
    Limit { limit: Option<u64>, offset: u64 },
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
        if !node.cte_map.0.is_empty() {
            return Err(not_implemented("WITH"));
        }
        match &node.kind {
            QueryNodeKind::Select(select) => self.bind_select_node(select, &node.modifiers),
            QueryNodeKind::SetOperation(_) => Err(not_implemented("UNION, EXCEPT and INTERSECT")),
            _ => Err(not_implemented("this kind of query")),
        }
    }

    /// `Binder::BindNode(SelectNode)` + `CreatePlan(BoundSelectNode)` + `VisitQueryNode`.
    fn bind_select_node(&mut self, node: &SelectNode, modifiers: &[ResultModifier]) -> Result<BoundStatement> {
        if !node.groups.group_expressions.is_empty()
            || !node.groups.grouping_sets.is_empty()
            || node.force_aggregates
            || node.having.is_some()
        {
            return Err(not_implemented("GROUP BY"));
        }
        if node.qualify.is_some() {
            return Err(not_implemented("QUALIFY"));
        }
        if node.sample.is_some() {
            return Err(not_implemented("SAMPLE"));
        }
        let mut plan = self.bind_table_ref(&node.from_table)?;
        let mut select_list = self.expand_star_expressions(&node.select_list)?;
        if select_list.is_empty() {
            return Err(binder_error("SELECT list is empty after resolving * expressions!"));
        }
        // SelectBindState: names, aliases and the (qualified) select list entries
        let mut names = Vec::with_capacity(select_list.len());
        let mut state = SelectBindState::default();
        for (i, expr) in select_list.iter_mut().enumerate() {
            names.push(expression_name(expr));
            self.qualify_column_names(expr);
            if !expr.alias.is_empty() {
                state.alias_map.push((expr.alias.clone(), i));
            }
            state.projection_map.push((without_aliases(expr), i));
            state.original_expressions.push(expr.clone());
        }
        let column_count = select_list.len();
        // PrepareModifiers: ORDER BY and DISTINCT targets refer to select list entries,
        // appending the ones that are not there yet
        let mut prepared = Vec::new();
        for modifier in modifiers {
            prepared.push(self.prepare_modifier(modifier, &mut select_list, &mut state)?);
        }
        if let Some(condition) = &node.where_clause {
            // the WHERE clause may refer to aliases of the select list (`ColumnAliasBinder`)
            self.alias_fallback = state
                .alias_map
                .iter()
                .map(|(alias, i)| (alias.clone(), select_list[*i].clone()))
                .collect();
            let condition = self.bind_expression(condition);
            self.alias_fallback.clear();
            let condition = condition?.cast_to(&LogicalType::BOOLEAN, false);
            plan = LogicalOperator::new(
                LogicalOperatorKind::Filter {
                    expressions: vec![condition],
                },
                vec![plan],
            );
        }
        // a select list entry may refer to the aliases of the entries before it
        let mut expressions = Vec::with_capacity(select_list.len());
        for (i, expr) in select_list.iter().enumerate() {
            self.alias_fallback = state
                .alias_map
                .iter()
                .filter(|(_, j)| *j < i)
                .map(|(alias, j)| (alias.clone(), select_list[*j].clone()))
                .collect();
            let bound = self.bind_expression(expr);
            self.alias_fallback.clear();
            expressions.push(bound?);
        }
        let types: Vec<LogicalType> = expressions.iter().map(|e| e.return_type.clone()).collect();
        let projection_index = self.generate_table_index();
        let mut plan = LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index: projection_index,
                expressions,
            },
            vec![plan],
        );
        // BindModifiers + VisitQueryNode: the modifiers in the order they were written
        let column = |i: usize| -> Result<BoundExpr> {
            if i >= types.len() {
                return Err(binder_error(format!(
                    "ORDER term out of range - should be between 1 and {}",
                    types.len()
                )));
            }
            let name = names.get(i).cloned().unwrap_or_default();
            Ok(BoundExpr::column_ref(
                ColumnBinding::new(projection_index, i),
                types[i].clone(),
                name,
            ))
        };
        for modifier in prepared {
            let kind = match modifier {
                PreparedModifier::Distinct => LogicalOperatorKind::Distinct {
                    targets: (0..column_count).map(column).collect::<Result<_>>()?,
                },
                PreparedModifier::Order(orders) => LogicalOperatorKind::Order {
                    orders: orders
                        .into_iter()
                        .map(|(modifiers, index)| {
                            Ok(BoundOrderByNode {
                                modifiers,
                                expr: column(index)?,
                            })
                        })
                        .collect::<Result<_>>()?,
                },
                PreparedModifier::OrderAll(modifiers) => LogicalOperatorKind::Order {
                    orders: (0..column_count)
                        .map(|i| {
                            Ok(BoundOrderByNode {
                                modifiers,
                                expr: column(i)?,
                            })
                        })
                        .collect::<Result<_>>()?,
                },
                PreparedModifier::Limit { limit, offset } => LogicalOperatorKind::Limit { limit, offset },
            };
            plan = LogicalOperator::new(kind, vec![plan]);
        }
        // prune the entries ORDER BY added to the select list
        if types.len() > column_count {
            let prune_index = self.generate_table_index();
            let expressions = (0..column_count).map(column).collect::<Result<_>>()?;
            plan = LogicalOperator::new(
                LogicalOperatorKind::Projection {
                    table_index: prune_index,
                    expressions,
                },
                vec![plan],
            );
        }
        Ok(BoundStatement {
            plan,
            types: types[..column_count].to_vec(),
            names,
            return_type: StatementReturnType::QueryResult,
        })
    }

    /// `ExpressionBinder::QualifyColumnNames`: qualifies the column references that bind to a
    /// table with that table's alias, so that equal expressions compare equal.
    fn qualify_column_names(&self, expr: &mut Expr) {
        visit_expr_mut(expr, &mut |e| {
            if let ExprKind::ColumnRef(names) = &mut e.kind
                && let [column] = names.as_slice()
                && let Ok((binding, ..)) = self.bind_context.bind_column(column)
                && let Some(table) = self
                    .bind_context
                    .bindings
                    .iter()
                    .find(|b| b.table_index == binding.table_index)
            {
                *names = vec![table.alias.clone(), table.names[binding.column_index].clone()];
            }
        });
    }

    /// `Binder::PrepareModifiers` for one modifier.
    fn prepare_modifier(
        &mut self,
        modifier: &ResultModifier,
        select_list: &mut Vec<Expr>,
        state: &mut SelectBindState,
    ) -> Result<PreparedModifier> {
        Ok(match modifier {
            ResultModifier::Distinct(targets) if targets.is_empty() => PreparedModifier::Distinct,
            ResultModifier::Distinct(_) => return Err(not_implemented("DISTINCT ON")),
            ResultModifier::Order(nodes) => {
                if let [OrderByNode { expr, order, nulls }] = nodes.as_slice()
                    && let ExprKind::Star(star) = &expr.kind
                    && star.exclude_list.is_empty()
                    && star.replace_list.is_empty()
                    && star.expr.is_none()
                {
                    // ORDER BY ALL
                    return Ok(PreparedModifier::OrderAll(self.options.resolve_order(*order, *nulls)));
                }
                let mut orders = Vec::with_capacity(nodes.len());
                for node in nodes {
                    if matches!(node.expr.kind, ExprKind::Star(_)) {
                        return Err(not_implemented("ORDER BY with a star expression"));
                    }
                    let index = self.bind_order_expression(&node.expr, select_list, state)?;
                    orders.push((self.options.resolve_order(node.order, node.nulls), index));
                }
                PreparedModifier::Order(orders)
            }
            ResultModifier::Limit { limit, offset, percent } => {
                if *percent {
                    return Err(not_implemented("LIMIT with a percentage"));
                }
                let limit = match limit {
                    Some(limit) => self.bind_limit_value(limit, false)?,
                    None => None,
                };
                let offset = match offset {
                    Some(offset) => self.bind_limit_value(offset, true)?.unwrap_or(0),
                    None => 0,
                };
                PreparedModifier::Limit { limit, offset }
            }
        })
    }

    /// `Binder::BindLimitValue` for constant values: NULL means no limit (or no offset).
    fn bind_limit_value(&mut self, expr: &Expr, is_offset: bool) -> Result<Option<u64>> {
        let mut binder = Binder::new(self.catalog);
        let bound = binder.bind_expression(expr)?;
        if !bound.is_foldable() {
            return Err(not_implemented(
                "LIMIT and OFFSET with expressions that are not constant",
            ));
        }
        let value = evaluate_scalar(&bound.cast_to(&LogicalType::BIGINT, false))?;
        if value.is_null() {
            return Ok(if is_offset { Some(0) } else { None });
        }
        let value = value.as_i128().expect("a BIGINT");
        if value < 0 {
            return Err(binder_error("LIMIT/OFFSET cannot be negative"));
        }
        Ok(Some(value as u64))
    }

    /// `OrderBinder::Bind`: the select list entry an ORDER BY expression refers to, appending
    /// the expression to the select list when it is not there.
    fn bind_order_expression(
        &self,
        expr: &Expr,
        select_list: &mut Vec<Expr>,
        state: &mut SelectBindState,
    ) -> Result<usize> {
        match &expr.kind {
            ExprKind::Constant(literal) => {
                // ORDER BY an integer refers to a select list entry; other constants are an error
                if literal.kind != LiteralKind::Integer {
                    return Err(binder_error(
                        "ORDER BY non-integer literal has no effect.\n* SET order_by_non_integer_literal=true to \
                         allow this behavior.",
                    ));
                }
                let value: i64 = literal.text.replace('_', "").parse().unwrap_or(i64::MAX);
                return Ok(if value <= 0 {
                    i64::MAX as usize
                } else {
                    value as usize - 1
                });
            }
            ExprKind::Positional(index) => return Ok(*index as usize - 1),
            ExprKind::ColumnRef(names) if names.len() == 1 => {
                let name = &names[0];
                // an alias of the select list
                if let Some((_, i)) = state.alias_map.iter().find(|(a, _)| a.eq_ignore_ascii_case(name)) {
                    return Ok(*i);
                }
                // a column the select list names without an alias
                let matching: Vec<usize> = state
                    .original_expressions
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        e.alias.is_empty()
                            && matches!(&e.kind, ExprKind::ColumnRef(n) if n.last().is_some_and(|c| c == name))
                    })
                    .map(|(i, _)| i)
                    .collect();
                if let [i] = matching.as_slice() {
                    return Ok(*i);
                }
            }
            ExprKind::Parameter(_) => {
                return Err(Error::new(
                    ErrorKind::ParameterNotAllowed,
                    "Parameter not supported in ORDER BY clause",
                ));
            }
            ExprKind::Collate { .. } => return Err(not_implemented("COLLATE")),
            _ => {}
        }
        // the general case: an expression of the select list, or a new entry
        let mut qualified = expr.clone();
        self.qualify_column_names(&mut qualified);
        let key = without_aliases(&qualified);
        if let Some((_, i)) = state.projection_map.iter().rev().find(|(e, _)| *e == key) {
            return Ok(*i);
        }
        let index = select_list.len();
        state.projection_map.push((key, index));
        select_list.push(qualified);
        Ok(index)
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
