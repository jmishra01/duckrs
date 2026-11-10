// Ported from duckdb/src/planner/binder/query_node/{bind_select_node,plan_select_node}.cpp,
// duckdb/src/planner/binder/tableref/{bind_basetableref,bind_expressionlistref,
// bind_emptytableref}.cpp, duckdb/src/planner/binder/statement/{bind_select,bind_insert,
// bind_create_table}.cpp and duckdb/src/planner/bind_context.cpp (alias and star expansion)
// @ rust-port-baseline
//
//! Binding SELECT (one table or VALUES, WHERE, a select list, ORDER BY, LIMIT/OFFSET and
//! DISTINCT), INSERT and CREATE TABLE.

use super::expression::{checked_cast, without_aliases};
use super::generated::{physical_constraints, table_binding};
use super::window::WindowBindState;
use super::{
    AggregateBindState, BindContext, Binder, Binding, BoundStatement, ColumnMode, StatementReturnType, WorkingTable,
    binder_error,
};
use crate::catalog::{
    ColumnDefinition, DEFAULT_CATALOG, OnCreateConflict, SchemaCatalogEntry, TEMP_SCHEMA, TableCatalogEntry,
    TableOrView, ViewCatalogEntry,
};
use crate::execution::expression_executor::evaluate_scalar;
use crate::function::binder::expression_return_type;
use crate::function::combine_types::{force_max_logical_type, normalize_type, try_max_logical_type};
use crate::function::generic::is_unnest_call;
use crate::optimizer::optimize;
use crate::planner::column_binding_resolver::resolve_column_bindings;
use crate::planner::expression::{BoundExpr, BoundExprKind, ColumnBinding};
use crate::planner::logical::{
    BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID, CteKeyColumn, LogicalOperator, LogicalOperatorKind,
};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::sort_key::OrderModifiers;
use duckrs_common::string_util::{candidates_message, similarity_rating, top_n_strings};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use duckrs_parser::ast::UpdateStatement;
use duckrs_parser::ast::{
    CreateTableInfo, CteMap, DeleteStatement, Expr, ExprKind, InsertStatement, InsertValues, Literal, LiteralKind,
    OnCreateConflict as AstConflict, OrderByNode, QualifiedName, QueryNode, QueryNodeKind, RecursiveCteNode,
    ResultModifier, SelectNode, SelectStatement, SetOpType, SetOperationNode, TableRef, TableRefKind,
    sql_quoted_identifier, values_list,
};
use duckrs_parser::transformer::visit_expr_mut;
use duckrs_parser::{ast, parse_ast};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn excluded_in_returning() -> Error {
    Error::new(
        ErrorKind::NotImplemented,
        "'excluded' qualified columns are not supported in the RETURNING clause yet",
    )
}

pub(super) fn not_implemented(what: &str) -> Error {
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

/// `GroupBinder::ReplaceSelectRef`: a group naming a select list entry (by alias or position)
/// takes that entry's expression, and the entry becomes a reference to the group.
fn replace_select_ref(
    group: &mut Expr,
    group_index: usize,
    select_list: &mut [Expr],
    state: &SelectBindState,
    aggregates: &mut AggregateBindState,
    used_select_entries: &mut Vec<usize>,
) -> Result<()> {
    let select_index = match &group.kind {
        ExprKind::ColumnRef(names) if names.len() == 1 || (names.len() == 2 && names[0] == "alias") => {
            let name = names.last().expect("checked above");
            let Some((alias, index)) = state.alias_map.iter().find(|(a, _)| a.eq_ignore_ascii_case(name)) else {
                return Ok(());
            };
            aggregates.group_alias_map.push((alias.clone(), group_index));
            *index
        }
        ExprKind::Constant(literal) if literal.kind == LiteralKind::Integer => {
            let Ok(position) = literal.text.replace('_', "").parse::<i64>() else {
                return Ok(());
            };
            (position - 1) as usize
        }
        ExprKind::Parameter(_) => {
            return Err(Error::new(
                ErrorKind::ParameterNotAllowed,
                "Parameter not supported in GROUP BY clause",
            ));
        }
        _ => return Ok(()),
    };
    if used_select_entries.contains(&select_index) {
        // grouping on the same entry twice: the second group has no effect
        *group = Expr::from(ExprKind::Constant(Literal::integer(42)));
        return Ok(());
    }
    if select_index >= select_list.len() {
        return Err(binder_error(format!(
            "GROUP BY term out of range - should be between 1 and {}",
            select_list.len()
        )));
    }
    // the entry becomes a reference to the group, named after its position
    let position = select_index.to_string();
    *group = std::mem::replace(
        &mut select_list[select_index],
        ExprKind::ColumnRef(vec![position.clone()]).into(),
    );
    aggregates.group_alias_map.push((position, group_index));
    used_select_entries.push(select_index);
    Ok(())
}

/// `Binder::BuildUnionByNameInfo`: the names and types of the result, and for each child the
/// child's column that makes each result column.
#[allow(clippy::type_complexity)]
fn union_by_name_columns(
    children: &[BoundStatement],
) -> Result<(Vec<String>, Vec<LogicalType>, Vec<Vec<Option<usize>>>)> {
    let mut names: Vec<String> = Vec::new();
    for child in children {
        for (i, name) in child.names.iter().enumerate() {
            if child.names[..i].iter().any(|n| n.eq_ignore_ascii_case(name)) {
                return Err(binder_error(format!(
                    "UNION (ALL) BY NAME operation doesn't support duplicate names in the SELECT list - the name \
                     \"{name}\" occurs multiple times"
                )));
            }
            if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                names.push(name.clone());
            }
        }
    }
    let position =
        |child: &BoundStatement, name: &String| child.names.iter().position(|n| n.eq_ignore_ascii_case(name));
    let mut types = Vec::with_capacity(names.len());
    for name in &names {
        let mut ty: Option<LogicalType> = None;
        for child in children {
            if let Some(i) = position(child, name) {
                ty = Some(match ty {
                    Some(ty) => force_max_logical_type(&ty, &child.types[i]),
                    None => child.types[i].clone(),
                });
            }
        }
        types.push(ty.expect("a name comes from a child"));
    }
    let sources = children
        .iter()
        .map(|child| names.iter().map(|name| position(child, name)).collect())
        .collect();
    Ok((names, types, sources))
}

/// A result modifier with its targets resolved to select list indexes.
enum PreparedModifier {
    Distinct,
    /// `DISTINCT ON`: the indexes of its targets
    DistinctOn(Vec<usize>),
    /// `LIMIT x%`
    LimitPercent {
        percent: f64,
        offset: u64,
    },
    Order(Vec<(OrderModifiers, usize)>),
    OrderAll(OrderModifiers),
    Limit {
        limit: Option<u64>,
        offset: u64,
        failure: Option<Error>,
    },
}

/// `GenerateRangeParameters`: the first value, the number of values and the increment.
pub(crate) fn range_parameters(values: &[i128], generate_series: bool) -> Result<(i128, u64, i128)> {
    let (start, mut end) = match values {
        [end] => (0, *end),
        [start, end, ..] => (*start, *end),
        [] => unreachable!("checked by the caller"),
    };
    let increment = values.get(2).copied().unwrap_or(1);
    if increment == 0 {
        return Err(binder_error("interval cannot be 0!"));
    }
    if (start > end && increment > 0) || (start < end && increment < 0) {
        return Ok((start, 0, increment));
    }
    if generate_series {
        // generate_series has inclusive bounds on the RHS
        end += if increment < 0 { -1 } else { 1 };
    }
    let difference = end - start;
    let mut count = difference / increment;
    if difference % increment != 0 {
        count += 1;
    }
    Ok((start, u64::try_from(count).unwrap_or(u64::MAX), increment))
}

/// `BindContext::AliasColumnNames`: column aliases first, and duplicate names made unique.
fn alias_column_names(table: &str, names: &[String], aliases: &[String]) -> Result<Vec<String>> {
    if aliases.len() > names.len() {
        return Err(binder_error(format!(
            "table {} has {} columns available but {} columns specified",
            sql_quoted_identifier(table),
            names.len(),
            aliases.len()
        )));
    }
    let mut result: Vec<String> = Vec::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        let base = aliases.get(i).unwrap_or(name);
        let mut candidate = base.clone();
        let mut index = 1;
        while result.iter().any(|n| n.eq_ignore_ascii_case(&candidate)) {
            candidate = format!("{base}_{index}");
            index += 1;
        }
        result.push(candidate);
    }
    Ok(result)
}

/// The schema of a qualified table name, checking that its catalog is ours.
/// The schema a name is qualified with (`Binder::BindTableName`): the key of a nested one when
/// the qualification is a path of schemas. A leading component that is the catalog goes.
pub fn schema_of(name: &QualifiedName) -> Result<Option<String>> {
    match name.qualification.as_slice() {
        [] => Ok(None),
        [schema] => Ok(Some(schema.clone())),
        [catalog, _] if catalog.eq_ignore_ascii_case(TEMP_SCHEMA) => Ok(Some(TEMP_SCHEMA.to_string())),
        [catalog, schema] if catalog.eq_ignore_ascii_case(DEFAULT_CATALOG) => Ok(Some(schema.clone())),
        [catalog, path @ ..] if catalog.eq_ignore_ascii_case(DEFAULT_CATALOG) => {
            Ok(Some(crate::catalog::schema_key(path)))
        }
        path => Ok(Some(crate::catalog::schema_key(path))),
    }
}

impl Binder<'_> {
    /// `schema_of` in this binder's catalog, which a qualification can name by its own name, as
    /// the body of a view of an attached database does.
    pub(super) fn schema_of(&self, name: &QualifiedName) -> Result<Option<String>> {
        let database = self
            .session
            .as_ref()
            .map_or(self.catalog.name.as_str(), |s| s.database.as_str());
        if name.qualification.len() >= 2
            && name.qualification[0].eq_ignore_ascii_case(database)
            && !name.qualification[0].eq_ignore_ascii_case(DEFAULT_CATALOG)
        {
            let mut rest = name.clone();
            rest.qualification[0] = DEFAULT_CATALOG.to_string();
            return schema_of(&rest);
        }
        schema_of(name)
    }

    /// `Binder::Bind(SelectStatement)`.
    pub(super) fn bind_select_statement(&mut self, select: &SelectStatement) -> Result<BoundStatement> {
        self.bind_query_node(&select.node)
    }

    /// `Binder::BindNode(QueryNode)`: the CTEs of a query are visible for what the query binds.
    /// They are not materialized: each reference binds and plans the CTE's query again.
    pub(super) fn bind_query_node(&mut self, node: &QueryNode) -> Result<BoundStatement> {
        let scope = self.ctes.len();
        for (name, cte) in &node.cte_map.0 {
            match &cte.query.kind {
                QueryNodeKind::Select(_) | QueryNodeKind::SetOperation(_) => {}
                QueryNodeKind::RecursiveCte(_) => {}
                _ => return Err(not_implemented("data-modifying statements in WITH")),
            }
            self.ctes
                .push((name.clone(), Arc::new(cte.clone()), self.outer_contexts.len()));
        }
        let result = match &node.kind {
            QueryNodeKind::Select(select) => self.bind_select_node(select, &node.modifiers),
            QueryNodeKind::SetOperation(operation) => self.bind_set_operation(operation, &node.modifiers),
            _ => Err(not_implemented("this kind of query")),
        };
        self.ctes.truncate(scope);
        result
    }

    /// A reference to the CTE at `index` of the visible ones (`Binder::Bind(BaseTableRef)`).
    fn bind_cte_reference(&mut self, table_ref: &TableRef, index: usize) -> Result<LogicalOperator> {
        let (name, cte, outer_depth) = self.ctes[index].clone();
        if cte.aliases.iter().any(String::is_empty) {
            return Err(binder_error("column alias cannot be empty"));
        }
        let mut binder = self.child_binder();
        // a CTE sees the ones before it, and not itself
        binder.ctes.truncate(index);
        binder.ctes_being_bound.push(name.clone());
        // a CTE sees what is around its definition, and not what is around a reference to it
        binder.outer_contexts.truncate(outer_depth);
        let bound = match &cte.query.kind {
            QueryNodeKind::RecursiveCte(node) => binder.bind_recursive_cte(&name, node, &cte.aliases)?,
            _ => binder.bind_query_node(&cte.query)?,
        };
        self.next_table_index = binder.next_table_index;
        // the aliases of the reference override the ones of the CTE
        let mut column_aliases = cte.aliases.clone();
        for (i, alias) in table_ref.column_name_alias.iter().enumerate() {
            match column_aliases.get_mut(i) {
                Some(column) => column.clone_from(alias),
                None => column_aliases.push(alias.clone()),
            }
        }
        let alias = if table_ref.alias.is_empty() {
            name
        } else {
            table_ref.alias.clone()
        };
        self.add_query_binding(alias, bound, &column_aliases)
    }

    /// `Binder::BindNode(RecursiveCTENode)`: the recursive part sees the CTE as the rows of the
    /// last round, of the types of the anchor.
    fn bind_recursive_cte(
        &mut self,
        name: &str,
        node: &RecursiveCteNode,
        aliases: &[String],
    ) -> Result<BoundStatement> {
        let mut anchor_binder = self.child_binder();
        let anchor = anchor_binder.bind_query_node(&node.left)?;
        self.next_table_index = anchor_binder.next_table_index;
        let types = anchor.types.clone();
        let mut names = anchor.names.clone();
        for (column, alias) in names.iter_mut().zip(aliases) {
            column.clone_from(alias);
        }
        let key = bind_cte_key(&node.key_targets, &names)?;
        let working = Arc::new(Mutex::new(ColumnDataCollection::new(types.clone())));
        let recurring = Arc::new(Mutex::new(ColumnDataCollection::new(types.clone())));
        let used = Arc::new(AtomicBool::new(false));
        let mut binder = self.child_binder();
        binder.working_tables.push(WorkingTable {
            name: name.to_string(),
            names: names.clone(),
            types: types.clone(),
            rows: working.clone(),
            recurring: recurring.clone(),
            used: used.clone(),
            transition: false,
        });
        let recursive = binder.bind_query_node(&node.right)?;
        self.next_table_index = binder.next_table_index;
        if recursive.types.len() != types.len() {
            return Err(binder_error(
                "Set operations can only apply to expressions with the same number of result columns",
            ));
        }
        let columns = (0..types.len()).map(Some).collect();
        let recursive = self.cast_plan(recursive, &types, columns);
        let plan = if used.load(Ordering::Relaxed) || !key.is_empty() {
            let mut recursive = optimize(recursive, self.options.optimizer);
            let unresolved = Box::new(recursive.clone());
            resolve_column_bindings(&mut recursive)?;
            LogicalOperator::new(
                LogicalOperatorKind::RecursiveCte {
                    table_index: self.generate_table_index(),
                    types: types.clone(),
                    union_all: node.union_all,
                    working,
                    recurring,
                    key,
                    recursive: Box::new(recursive),
                    unresolved,
                },
                vec![anchor.plan],
            )
        } else {
            // WITH RECURSIVE that does not recurse is a plain set operation
            let table_index = self.generate_table_index();
            let plan = LogicalOperator::new(
                LogicalOperatorKind::Union {
                    table_index,
                    types: types.clone(),
                },
                vec![anchor.plan, recursive],
            );
            let distinct = if node.union_all {
                Vec::new()
            } else {
                vec![PreparedModifier::Distinct]
            };
            self.apply_modifiers(plan, distinct, table_index, &names, &types, types.len())?
        };
        let column_aliases = names.iter().cloned().zip(0..).collect();
        Ok(BoundStatement {
            plan,
            names,
            types,
            column_aliases,
            return_type: StatementReturnType::QueryResult,
        })
    }

    /// `Binder::BindNode(SetOperationNode)`.
    fn bind_set_operation(&mut self, node: &SetOperationNode, modifiers: &[ResultModifier]) -> Result<BoundStatement> {
        let mut children = Vec::with_capacity(node.children.len());
        for child in &node.children {
            let mut binder = self.child_binder();
            binder.ctes = self.ctes.clone();
            let bound = binder.bind_query_node(child)?;
            self.next_table_index = binder.next_table_index;
            children.push(bound);
        }
        let by_name = node.setop_type == SetOpType::UnionByName;
        let (names, types, sources) = if by_name {
            union_by_name_columns(&children)?
        } else {
            let columns = children[0].types.len();
            if children.iter().any(|c| c.types.len() != columns) {
                return Err(binder_error(
                    "Set operations can only apply to expressions with the same number of result columns",
                ));
            }
            let types = (0..columns)
                .map(|i| {
                    children[1..].iter().fold(children[0].types[i].clone(), |ty, child| {
                        force_max_logical_type(&ty, &child.types[i])
                    })
                })
                .collect::<Vec<_>>();
            let sources = children.iter().map(|_| (0..columns).map(Some).collect()).collect();
            (children[0].names.clone(), types, sources)
        };
        // an alias of any of the children can name a column in ORDER BY
        let mut aliases: Vec<(String, usize)> = Vec::new();
        for child in &children {
            for (name, i) in &child.column_aliases {
                let index = if by_name {
                    names.iter().position(|n| n == name).unwrap_or(*i)
                } else {
                    *i
                };
                if !aliases.iter().any(|(a, _)| a.eq_ignore_ascii_case(name)) {
                    aliases.push((name.clone(), index));
                }
            }
        }
        let mut plans = Vec::with_capacity(children.len());
        for (child, columns) in children.into_iter().zip(sources) {
            plans.push(self.cast_plan(child, &types, columns));
        }
        let table_index = self.generate_table_index();
        let kind = match node.setop_type {
            SetOpType::Union | SetOpType::UnionByName => LogicalOperatorKind::Union {
                table_index,
                types: types.clone(),
            },
            operation => {
                // the rows match as the collations of their columns say
                let mut keys = Vec::with_capacity(types.len());
                let mut collated = false;
                for (i, ty) in types.iter().enumerate() {
                    let column = BoundExpr::new(BoundExprKind::Reference(i), ty.clone());
                    let (key, pushed) = crate::planner::collation::push_collation(
                        column,
                        ty,
                        &self.default_collation(),
                        crate::planner::collation::CollationType::AllCollations,
                    )?;
                    collated |= pushed;
                    keys.push(key);
                }
                LogicalOperatorKind::ExceptIntersect {
                    table_index,
                    types: types.clone(),
                    except: operation == SetOpType::Except,
                    all: node.setop_all,
                    keys: if collated { keys } else { Vec::new() },
                }
            }
        };
        let plan = LogicalOperator::new(kind, plans);
        let mut prepared = Vec::with_capacity(modifiers.len() + 1);
        if !node.setop_all {
            prepared.push(PreparedModifier::Distinct);
        }
        for modifier in modifiers {
            prepared.push(self.prepare_set_operation_modifier(modifier, &aliases, &types)?);
        }
        let plan = self.apply_modifiers(plan, prepared, table_index, &names, &types, types.len())?;
        Ok(BoundStatement {
            plan,
            column_aliases: aliases,
            names,
            types,
            return_type: StatementReturnType::QueryResult,
        })
    }

    /// `CastLogicalOperatorToTypes`: a projection of a child of a set operation. `columns[i]` is
    /// the child's column that makes column `i`, or `None` for NULL.
    pub(super) fn cast_plan(
        &mut self,
        child: BoundStatement,
        types: &[LogicalType],
        columns: Vec<Option<usize>>,
    ) -> LogicalOperator {
        let in_order = columns.iter().enumerate().all(|(i, c)| *c == Some(i)) && columns.len() == child.types.len();
        if in_order && child.types == types {
            return child.plan;
        }
        let bindings = child.plan.column_bindings();
        let expressions = columns
            .iter()
            .zip(types)
            .map(|(column, ty)| match column {
                Some(i) => BoundExpr::column_ref(bindings[*i], child.types[*i].clone(), child.names[*i].clone())
                    .cast_to(ty, false),
                None => BoundExpr::constant(Value::null(ty.clone())),
            })
            .collect();
        LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index: self.generate_table_index(),
                expressions,
            },
            vec![child.plan],
        )
    }

    /// `Binder::PrepareModifiers` for a set operation, whose ORDER BY can only name its columns.
    fn prepare_set_operation_modifier(
        &mut self,
        modifier: &ResultModifier,
        aliases: &[(String, usize)],
        types: &[LogicalType],
    ) -> Result<PreparedModifier> {
        let ResultModifier::Order(nodes) = modifier else {
            let mut unused = Vec::new();
            return self.prepare_modifier(modifier, &mut unused, &mut SelectBindState::default());
        };
        if matches!(nodes.as_slice(), [node] if matches!(node.expr.kind, ExprKind::Star(_))) {
            let node = &nodes[0];
            return Ok(PreparedModifier::OrderAll(
                self.options.resolve_order(node.order, node.nulls),
            ));
        }
        let mut orders = Vec::with_capacity(nodes.len());
        for node in nodes {
            let index = match &node.expr.kind {
                ExprKind::Constant(literal) if literal.kind == LiteralKind::Integer => {
                    let value: i64 = literal.text.replace('_', "").parse().unwrap_or(i64::MAX);
                    if value <= 0 || value as usize > types.len() {
                        return Err(binder_error(format!(
                            "ORDER term out of range - should be between 1 and {}",
                            types.len()
                        )));
                    }
                    value as usize - 1
                }
                ExprKind::Positional(index) => *index as usize - 1,
                _ => {
                    let name = match &node.expr.kind {
                        ExprKind::ColumnRef(names) => names.last().expect("a name").clone(),
                        _ => expression_name(&node.expr),
                    };
                    let found = aliases.iter().find(|(a, _)| a.eq_ignore_ascii_case(&name));
                    let Some((_, index)) = found else {
                        return Err(binder_error(format!(
                            "Could not ORDER BY column \"{}\": add the expression/function to every SELECT, or move \
                             the UNION into a FROM clause.",
                            node.expr
                        )));
                    };
                    *index
                }
            };
            orders.push((self.options.resolve_order(node.order, node.nulls), index));
        }
        Ok(PreparedModifier::Order(orders))
    }

    /// `VisitQueryNode`: the modifiers in the order they were written, on top of `plan`, whose
    /// columns are numbered by `table_index`.
    fn apply_modifiers(
        &mut self,
        mut plan: LogicalOperator,
        prepared: Vec<PreparedModifier>,
        table_index: usize,
        names: &[String],
        types: &[LogicalType],
        column_count: usize,
    ) -> Result<LogicalOperator> {
        let column = |i: usize| -> Result<BoundExpr> {
            if i >= types.len() {
                return Err(binder_error(format!(
                    "ORDER term out of range - should be between 1 and {}",
                    types.len()
                )));
            }
            let name = names.get(i).cloned().unwrap_or_default();
            Ok(BoundExpr::column_ref(
                ColumnBinding::new(table_index, i),
                types[i].clone(),
                name,
            ))
        };
        // DISTINCT ON keeps the first row of each group as ORDER BY sees them
        let mut prepared = prepared;
        if let Some(distinct) = prepared
            .iter()
            .position(|m| matches!(m, PreparedModifier::DistinctOn(_)))
            && let Some(order) = prepared
                .iter()
                .position(|m| matches!(m, PreparedModifier::Order(_) | PreparedModifier::OrderAll(_)))
            && order > distinct
        {
            prepared.swap(distinct, order);
        }
        // ORDER BY and DISTINCT see the values as their collation does
        let default_collation = self.default_collation();
        let collated_column = |i: usize| -> Result<BoundExpr> {
            let expr = column(i)?;
            let ty = expr.return_type.clone();
            let kind = crate::planner::collation::CollationType::AllCollations;
            Ok(crate::planner::collation::push_collation(expr, &ty, &default_collation, kind)?.0)
        };
        for modifier in prepared {
            let kind = match modifier {
                PreparedModifier::Distinct => LogicalOperatorKind::Distinct {
                    targets: (0..column_count).map(collated_column).collect::<Result<_>>()?,
                },
                PreparedModifier::DistinctOn(indexes) => LogicalOperatorKind::Distinct {
                    targets: indexes.into_iter().map(collated_column).collect::<Result<_>>()?,
                },
                PreparedModifier::LimitPercent { percent, offset } => {
                    LogicalOperatorKind::LimitPercent { percent, offset }
                }
                PreparedModifier::Order(orders) => LogicalOperatorKind::Order {
                    orders: orders
                        .into_iter()
                        .map(|(modifiers, index)| {
                            Ok(BoundOrderByNode {
                                modifiers,
                                expr: collated_column(index)?,
                            })
                        })
                        .collect::<Result<_>>()?,
                },
                PreparedModifier::OrderAll(modifiers) => LogicalOperatorKind::Order {
                    orders: (0..column_count)
                        .map(|i| {
                            Ok(BoundOrderByNode {
                                modifiers,
                                expr: collated_column(i)?,
                            })
                        })
                        .collect::<Result<_>>()?,
                },
                // LIMIT 0 needs nothing from its input, however expensive that is
                // (`LimitPushdown`/`EmptyResultPullup` in the optimizer)
                PreparedModifier::Limit { limit: Some(0), .. } => LogicalOperatorKind::EmptyResult {
                    types: plan.types(),
                    bindings: plan.column_bindings(),
                },
                PreparedModifier::Limit { limit, offset, failure } => {
                    LogicalOperatorKind::Limit { limit, offset, failure }
                }
            };
            let children = if matches!(kind, LogicalOperatorKind::EmptyResult { .. }) {
                vec![]
            } else {
                vec![plan]
            };
            plan = LogicalOperator::new(kind, children);
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
        Ok(plan)
    }

    /// `Binder::BindNode(SelectNode)` + `CreatePlan(BoundSelectNode)` + `VisitQueryNode`.
    /// `PlanUnnest`: takes the `unnest` calls out of `expressions` and puts a `LogicalUnnest` on
    /// top of `plan` for them. An unnest of an unnest needs another one below.
    fn plan_unnests(&mut self, expressions: &mut [BoundExpr], plan: LogicalOperator) -> LogicalOperator {
        let table_index = self.generate_table_index();
        let mut lists: Vec<BoundExpr> = Vec::new();
        for expression in expressions.iter_mut() {
            expression.walk_mut(&mut |e| {
                if !is_unnest_call(e) {
                    return;
                }
                let BoundExprKind::Function { children, .. } = &mut e.kind else {
                    unreachable!("an unnest is a function");
                };
                let list = children.pop().expect("unnest has an argument");
                let binding = ColumnBinding::new(table_index, lists.len());
                lists.push(list);
                let alias = std::mem::take(&mut e.alias);
                *e = BoundExpr::column_ref(binding, e.return_type.clone(), alias);
            });
        }
        if lists.is_empty() {
            return plan;
        }
        let plan = self.plan_unnests(&mut lists, plan);
        LogicalOperator::new(
            LogicalOperatorKind::Unnest {
                table_index,
                expressions: lists,
            },
            vec![plan],
        )
    }

    fn bind_select_node(&mut self, node: &SelectNode, modifiers: &[ResultModifier]) -> Result<BoundStatement> {
        let mut plan = self.bind_table_ref(&node.from_table)?;
        let mut select_list = Vec::new();
        for expr in self.expand_star_expressions(&node.select_list)? {
            match self.struct_unnest_fields(&expr) {
                Some(fields) => select_list.extend(fields),
                None => select_list.push(expr),
            }
        }
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
        // qualify the groups, and swap the ones naming a select list entry with that entry
        let group_index = self.generate_table_index();
        let aggregate_index = self.generate_table_index();
        let groupings_index = self.generate_table_index();
        let mut aggregates = AggregateBindState {
            group_index,
            aggregate_index,
            groupings_index,
            grouping_functions: Vec::new(),
            group_map: Vec::new(),
            group_alias_map: Vec::new(),
            groups: Vec::new(),
            aggregates: Vec::new(),
            aggregate_keys: Vec::new(),
            inside_aggregate: false,
            mode: ColumnMode::SelectList,
            bound_columns: Vec::new(),
            bound_aggregate: false,
            force_aggregates: node.force_aggregates,
            collated_groups: Vec::new(),
            grouping_set_count: node.groups.grouping_sets.len(),
        };
        let mut group_expressions = node.groups.group_expressions.clone();
        let mut used_select_entries = Vec::new();
        for (i, group) in group_expressions.iter_mut().enumerate() {
            self.qualify_column_names(group);
            replace_select_ref(
                group,
                i,
                &mut select_list,
                &state,
                &mut aggregates,
                &mut used_select_entries,
            )?;
            aggregates.group_map.push((without_aliases(group), i));
        }
        // PrepareModifiers: ORDER BY and DISTINCT targets refer to select list entries,
        // appending the ones that are not there yet
        let mut prepared = Vec::new();
        for modifier in modifiers {
            prepared.push(self.prepare_modifier(modifier, &mut select_list, &mut state)?);
        }
        let where_clause = node
            .where_clause
            .as_deref()
            .map(|condition| self.bind_where_star_expression(condition))
            .transpose()?;
        if let Some(condition) = &where_clause {
            // the WHERE clause may refer to aliases of the select list (`ColumnAliasBinder`)
            self.alias_fallback = state
                .alias_map
                .iter()
                .map(|(alias, i)| (alias.clone(), state.original_expressions[*i].clone()))
                .collect();
            self.aggregate_error = "WHERE clause cannot contain aggregates!";
            self.window_error = "WHERE clause cannot contain window functions!";
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
        if let Some(options) = &node.sample {
            plan = bind_sample(options, plan)?;
        }
        self.aggregate_error = "GROUP BY clause cannot contain aggregates!";
        self.window_error = "GROUP BY clause cannot contain window functions!";
        for group in &group_expressions {
            let mut alias_in_expression = None;
            visit_expr_mut(&mut group.clone(), &mut |e| {
                if let ExprKind::ColumnRef(names) = &e.kind
                    && let [name] = names.as_slice()
                    && state.alias_map.iter().any(|(a, _)| a.eq_ignore_ascii_case(name))
                {
                    alias_in_expression = Some(name.clone());
                }
            });
            if let Some(alias) = alias_in_expression {
                return Err(binder_error(format!(
                    "Alias with name {alias} exists, but aliases cannot be used as part of an expression in the \
                     GROUP BY"
                )));
            }
            let bound = self.bind_expression(group)?;
            // a collated group groups by its collated value, and a `first(x)` gives x as it was
            let ty = bound.return_type.clone();
            let (collated, requires_collation) = crate::planner::collation::push_collation(
                bound.clone(),
                &ty,
                &self.default_collation(),
                crate::planner::collation::CollationType::AllCollations,
            )?;
            if requires_collation {
                let set = crate::function::BuiltinFunctions::get()
                    .aggregate("first")
                    .expect("first is an aggregate");
                let first = crate::function::binder::bind_aggregate_function(set, vec![bound], None, false)?;
                let index = aggregates.aggregates.len();
                aggregates
                    .aggregate_keys
                    .push(format!("collated group {}", aggregates.groups.len()));
                aggregates.aggregates.push(first);
                aggregates.collated_groups.push((aggregates.groups.len(), index));
            }
            aggregates.groups.push(collated);
        }
        self.aggregates = Some(aggregates);
        let window_index = self.generate_table_index();
        self.windows = Some(WindowBindState {
            window_index,
            windows: Vec::new(),
        });
        // a select list entry may refer to the aliases of the entries before it
        let mut expressions = Vec::with_capacity(select_list.len());
        let mut group_by_all = Vec::new();
        for (i, expr) in select_list.iter().enumerate() {
            self.alias_fallback = state
                .alias_map
                .iter()
                .filter(|(_, j)| *j < i)
                .map(|(alias, j)| (alias.clone(), state.original_expressions[*j].clone()))
                .collect();
            self.later_select_aliases = Some(
                state
                    .alias_map
                    .iter()
                    .filter(|(_, j)| *j >= i)
                    .map(|(alias, _)| alias.clone())
                    .collect(),
            );
            let bound = self.bind_expression(expr);
            self.alias_fallback.clear();
            self.later_select_aliases = None;
            expressions.push(bound?);
            let aggregates = self.aggregates.as_mut().expect("set above");
            if node.force_aggregates && i < column_count {
                // GROUP BY ALL: an entry with columns becomes a group
                if !aggregates.bound_columns.is_empty() {
                    if aggregates.bound_aggregate {
                        return Err(binder_error("Cannot mix aggregates with non-aggregated columns!"));
                    }
                    group_by_all.push(i);
                }
                aggregates.bound_columns.clear();
                aggregates.bound_aggregate = false;
            }
        }
        {
            let default_collation = self.default_collation();
            let aggregates = self.aggregates.as_mut().expect("set above");
            for i in group_by_all {
                let ty = expressions[i].return_type.clone();
                let group = aggregates.groups.len();
                let reference =
                    BoundExpr::column_ref(ColumnBinding::new(group_index, group), ty.clone(), names[i].clone());
                let original = std::mem::replace(&mut expressions[i], reference);
                // a collated group: grouped by the collated value, selected as `first(x)`
                let (collated, requires_collation) = crate::planner::collation::push_collation(
                    original.clone(),
                    &ty,
                    &default_collation,
                    crate::planner::collation::CollationType::AllCollations,
                )?;
                if requires_collation {
                    let set = crate::function::BuiltinFunctions::get()
                        .aggregate("first")
                        .expect("first is an aggregate");
                    let first = crate::function::binder::bind_aggregate_function(set, vec![original], None, false)?;
                    let index = aggregates.aggregates.len();
                    aggregates.aggregate_keys.push(format!("collated group {group}"));
                    aggregates.aggregates.push(first);
                    aggregates.collated_groups.push((group, index));
                    expressions[i] =
                        BoundExpr::column_ref(ColumnBinding::new(aggregate_index, index), ty, names[i].clone());
                    aggregates.groups.push(collated);
                } else {
                    aggregates.groups.push(original);
                }
                aggregates
                    .group_map
                    .push((without_aliases(&state.original_expressions[i]), group));
            }
        }
        // the HAVING clause binds after the select list and its groups
        let having = match &node.having {
            Some(having) => {
                // an alias of the select list wins over a column of the FROM clause, unless that
                // column is a group (`ColumnQualifier::QualifyColumnName`)
                let having = self.qualified_with_aliases(having, &state);
                self.aggregates.as_mut().expect("set above").mode = ColumnMode::Having;
                let windows = self.windows.take();
                self.window_error = "HAVING clause cannot contain window functions!";
                let bound = self.bind_expression(&having);
                self.windows = windows;
                self.alias_fallback.clear();
                self.aggregates.as_mut().expect("set above").mode = ColumnMode::SelectList;
                Some(bound?.cast_to(&LogicalType::BOOLEAN, false))
            }
            None => None,
        };
        let qualify = match &node.qualify {
            Some(_) if node.force_aggregates => {
                return Err(binder_error("Combining QUALIFY with GROUP BY ALL is not supported yet"));
            }
            Some(qualify) => {
                let qualify = self.qualified_with_aliases(qualify, &state);
                let bound = self.bind_expression(&qualify);
                self.alias_fallback.clear();
                Some(bound?.cast_to(&LogicalType::BOOLEAN, false))
            }
            None => None,
        };
        let windows = self.windows.take().expect("set above").windows;
        if qualify.is_some() && windows.is_empty() {
            return Err(binder_error(
                "at least one window function must appear in the SELECT column or QUALIFY clause",
            ));
        }
        let aggregates = self.aggregates.take().expect("set above");
        let has_groups = !aggregates.groups.is_empty();
        let has_aggregates = !aggregates.aggregates.is_empty();
        let has_grouping_sets = !node.groups.grouping_sets.is_empty();
        if has_groups || has_aggregates || having.is_some() || has_grouping_sets {
            // a column outside of the groups and aggregates has no single value per group
            if let Some(column) = aggregates.bound_columns.first() {
                let column = sql_quoted_identifier(column);
                let mut error = format!(
                    "column {column} must appear in the GROUP BY clause or must be part of an aggregate function."
                );
                if node.force_aggregates {
                    error += "\nGROUP BY ALL will only group entries in the SELECT list. Add it to the SELECT list or \
                              GROUP BY this entry explicitly.";
                } else {
                    error += &format!(
                        "\nEither add it to the GROUP BY list, or use ANY_VALUE({column}) if the exact value of \
                         {column} is not important."
                    );
                }
                return Err(binder_error(error));
            }
        }
        if has_groups || has_aggregates || having.is_some() {
            plan = LogicalOperator::new(
                LogicalOperatorKind::Aggregate {
                    group_index,
                    aggregate_index,
                    groupings_index,
                    groups: aggregates.groups,
                    aggregates: aggregates.aggregates,
                    grouping_sets: node.groups.grouping_sets.clone(),
                    grouping_functions: aggregates.grouping_functions,
                },
                vec![plan],
            );
        } else if has_grouping_sets {
            // edge case: `SELECT 1 FROM t GROUP BY ()` has one group, and nothing to compute
            plan = LogicalOperator::new(
                LogicalOperatorKind::DummyScan {
                    table_index: group_index,
                },
                vec![],
            );
        }
        if let Some(having) = having {
            plan = LogicalOperator::new(
                LogicalOperatorKind::Filter {
                    expressions: vec![having],
                },
                vec![plan],
            );
        }
        if !windows.is_empty() {
            plan = LogicalOperator::new(
                LogicalOperatorKind::Window {
                    table_index: window_index,
                    windows,
                },
                vec![plan],
            );
        }
        if let Some(qualify) = qualify {
            plan = LogicalOperator::new(
                LogicalOperatorKind::Filter {
                    expressions: vec![qualify],
                },
                vec![plan],
            );
        }
        let plan = self.plan_unnests(&mut expressions, plan);
        let types: Vec<LogicalType> = expressions.iter().map(|e| e.return_type.clone()).collect();
        let projection_index = self.generate_table_index();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index: projection_index,
                expressions,
            },
            vec![plan],
        );
        let plan = self.apply_modifiers(plan, prepared, projection_index, &names, &types, column_count)?;
        let mut column_aliases: Vec<(String, usize)> = names.iter().cloned().zip(0..).collect();
        column_aliases.truncate(column_count);
        Ok(BoundStatement {
            plan,
            types: types[..column_count].to_vec(),
            names,
            column_aliases,
            return_type: StatementReturnType::QueryResult,
        })
    }

    /// A HAVING or QUALIFY clause with its columns qualified, and `alias_fallback` set for it: an
    /// alias of the select list wins over a column of the FROM clause, unless that column is a
    /// group (`ColumnQualifier::QualifyColumnName`).
    fn qualified_with_aliases(&mut self, clause: &Expr, state: &SelectBindState) -> Expr {
        let mut clause = clause.clone();
        let alias_names = state
            .alias_map
            .iter()
            .map(|(alias, _)| alias.clone())
            .filter(|alias| {
                let mut column = Expr::column(vec![alias.clone()]);
                self.qualify_column_names(&mut column);
                self.try_bind_group(&column).is_none()
            })
            .collect::<Vec<_>>();
        self.qualify_column_names_except(&mut clause, &alias_names);
        self.alias_fallback = state
            .alias_map
            .iter()
            .map(|(alias, i)| (alias.clone(), state.original_expressions[*i].clone()))
            .collect();
        clause
    }

    /// `ExpressionBinder::QualifyColumnNames`: qualifies the column references that bind to a
    /// table with that table's alias, so that equal expressions compare equal.
    pub(super) fn qualify_column_names(&self, expr: &mut Expr) {
        self.qualify_column_names_except(expr, &[]);
    }

    /// [`Self::qualify_column_names`], leaving the unqualified `aliases` alone
    /// (`BaseSelectBinder::ClaimsAlias`).
    fn qualify_column_names_except(&self, expr: &mut Expr, aliases: &[String]) {
        // the parameters of lambdas are no columns; they come before their bodies in this walk
        let mut parameters: Vec<String> = Vec::new();
        visit_expr_mut(expr, &mut |e| {
            if let ExprKind::Lambda { lhs, .. } = &e.kind {
                let mut lhs = (**lhs).clone();
                visit_expr_mut(&mut lhs, &mut |p| {
                    if let ExprKind::ColumnRef(names) = &p.kind {
                        parameters.extend(names.iter().map(|n| n.to_lowercase()));
                    }
                });
                return;
            }
            if let ExprKind::ColumnRef(names) = &e.kind
                && parameters.contains(&names[0].to_lowercase())
            {
                return;
            }
            let using = match &e.kind {
                ExprKind::ColumnRef(names) => match names.as_slice() {
                    [name] if !aliases.iter().any(|a| a.eq_ignore_ascii_case(name)) => {
                        self.bind_context.using_expression(name).ok().flatten()
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(using) = using {
                e.kind = using.kind;
                return;
            }
            if let ExprKind::Positional(position) = e.kind
                && let Ok(names) = self.bind_context.position_to_column(position)
            {
                e.kind = ExprKind::ColumnRef(names);
            }
            if let ExprKind::ColumnRef(names) = &mut e.kind
                && !matches!(names.as_slice(), [name] if aliases.iter().any(|a| a.eq_ignore_ascii_case(name)))
                && let Ok((binding, _, column)) = self.bind_context.bind_column_ref(names)
                && let Some(table) = self
                    .bind_context
                    .bindings
                    .iter()
                    .find(|b| b.table_index == binding.table_index)
            {
                *names = self.bind_context.qualified_names(table, &column);
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
            ResultModifier::Distinct(targets) => {
                let mut indexes = Vec::with_capacity(targets.len());
                for target in targets {
                    indexes.push(self.bind_order_expression(target, select_list, state)?);
                }
                PreparedModifier::DistinctOn(indexes)
            }
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
                    let offset = match offset {
                        Some(offset) => self.bind_limit_value(offset, true)?.unwrap_or(0),
                        None => 0,
                    };
                    let Some(limit) = limit else {
                        return Ok(PreparedModifier::Limit {
                            limit: None,
                            offset,
                            failure: None,
                        });
                    };
                    return self.bind_limit_percent(limit, offset);
                }
                // a LIMIT or OFFSET that fails to evaluate fails once a row reaches it
                let mut failure = None;
                let mut value = |expr: &Option<Box<Expr>>, is_offset: bool| -> Result<Option<u64>> {
                    let Some(expr) = expr else {
                        return Ok(None);
                    };
                    match self.bind_limit_value(expr, is_offset) {
                        Err(error) if !matches!(error.kind, ErrorKind::Binder | ErrorKind::Parser) => {
                            failure.get_or_insert(error);
                            Ok(None)
                        }
                        result => result,
                    }
                };
                let limit = value(limit, false)?;
                let offset = value(offset, true)?.unwrap_or(0);
                PreparedModifier::Limit { limit, offset, failure }
            }
        })
    }

    /// `Binder::BindLimitValue` for a percentage.
    fn bind_limit_percent(&mut self, expr: &Expr, offset: u64) -> Result<PreparedModifier> {
        let mut binder = self.child_binder();
        let bound = binder.bind_expression(expr)?;
        let value = evaluate_scalar(&bound.cast_to(&LogicalType::DOUBLE, false))?;
        // NULL is all of the rows
        let percent: f64 = if value.is_null() {
            100.0
        } else {
            value.try_to_string()?.parse().unwrap_or(f64::NAN)
        };
        if percent.is_nan() || !(0.0..=100.0).contains(&percent) {
            return Err(Error::new(
                ErrorKind::OutOfRange,
                "Limit percent out of range, should be between 0% and 100%",
            ));
        }
        Ok(PreparedModifier::LimitPercent { percent, offset })
    }

    /// `Binder::BindLimitValue` for constant values: NULL means no limit (or no offset).
    fn bind_limit_value(&mut self, expr: &Expr, is_offset: bool) -> Result<Option<u64>> {
        let mut binder = self.child_binder();
        binder.correlated = Arc::default();
        let bound = binder.bind_expression(expr)?;
        if !binder.correlated.lock().is_empty() {
            return Err(binder_error("Correlated columns not supported in LIMIT/OFFSET"));
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
        // `OrderBinder::Bind(CollateExpression)`: a collation over a position or alias applies to
        // the select list entry it names, which is sorted by as a hidden entry
        let rewritten;
        let expr = match &expr.kind {
            ExprKind::Collate { child, collation }
                if matches!(
                    &child.kind,
                    ExprKind::Constant(_) | ExprKind::Positional(_) | ExprKind::ColumnRef(_)
                ) =>
            {
                let index = self.bind_order_expression(child, select_list, state)?;
                match state.original_expressions.get(index) {
                    Some(entry) => {
                        let mut target = entry.clone();
                        target.alias.clear();
                        rewritten = Expr::from(ExprKind::Collate {
                            child: Box::new(target),
                            collation: collation.clone(),
                        });
                        &rewritten
                    }
                    None => expr,
                }
            }
            _ => expr,
        };
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
            // `IsPotentialAlias`: a plain name, or one qualified with the word "alias"
            ExprKind::ColumnRef(names) if names.len() == 1 || (names.len() == 2 && names[0] == "alias") => {
                let name = names.last().expect("checked above");
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

    /// The `BoundExpandedExpression` of `BindSelectNode`: an UNNEST of a STRUCT in the select list
    /// stands for a column per field of the struct.
    fn struct_unnest_fields(&mut self, expr: &Expr) -> Option<Vec<Expr>> {
        let ExprKind::Function(call) = &expr.kind else {
            return None;
        };
        if !crate::function::generic::is_unnest(&call.name.name)
            || !call.name.qualification.is_empty()
            || call.args.len() != 1
            || !call.args[0].name.is_empty()
        {
            return None;
        }
        let argument = &call.args[0].expr;
        let bound = self.bind_expression(argument).ok()?;
        if bound.return_type.id() != LogicalTypeId::Struct {
            return None;
        }
        let fields = bound.return_type.struct_children();
        if fields.is_empty() {
            return None;
        }
        Some(
            fields
                .iter()
                .map(|(name, _)| {
                    let mut extract = call.clone();
                    extract.name.name = "struct_extract".into();
                    extract.args = vec![
                        ast::FunctionArgument::positional(argument.clone()),
                        ast::FunctionArgument::positional(Expr::constant(ast::Literal::string(name.clone()))),
                    ];
                    let mut field = Expr::from(ExprKind::Function(extract));
                    field.alias = name.clone();
                    field
                })
                .collect(),
        )
    }

    /// `BindContext::GenerateAllColumnExpressions`: replaces `*` and `t.*` with the columns of
    /// the bindings they stand for.
    fn expand_star_expressions(&self, select_list: &[Expr]) -> Result<Vec<Expr>> {
        let mut result = Vec::new();
        for expr in select_list {
            self.expand_star_expression(expr, &mut result)?;
        }
        Ok(result)
    }

    /// `BindContext::GenerateAllColumnExpressions` for one star.
    pub(super) fn generate_all_columns(&self, star: &ast::StarExpr) -> Result<Vec<Expr>> {
        let mut result = Vec::new();
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
                // `struct.*`: the fields of a STRUCT column
                if let Some((binding, index)) = self.bind_context.matching_binding(&star.relation_name)? {
                    let ty = &binding.types[index];
                    if ty.id() != LogicalTypeId::Struct {
                        return Err(binder_error(format!(
                            "Cannot extract field from expression \"{}\" because it is not a struct",
                            Expr::from(ExprKind::Star(star.clone()))
                        )));
                    }
                    let mut excluded = vec![false; star.exclude_list.len()];
                    for (field, _) in ty.struct_children() {
                        if let Some(position) = star
                            .exclude_list
                            .iter()
                            .position(|e| e.qualification.is_empty() && e.column.eq_ignore_ascii_case(field))
                        {
                            excluded[position] = true;
                            continue;
                        }
                        let mut column =
                            Expr::column(vec![binding.alias.clone(), binding.names[index].clone(), field.clone()]);
                        column.alias = field.clone();
                        // HandleRename
                        if let Some((name, replacement)) =
                            star.replace_list.iter().find(|(n, _)| n.eq_ignore_ascii_case(field))
                        {
                            column = replacement.clone();
                            column.alias = name.clone();
                        }
                        if let Some((_, alias)) = star
                            .rename_list
                            .iter()
                            .find(|(n, _)| n.qualification.is_empty() && n.column.eq_ignore_ascii_case(field))
                        {
                            column.alias = alias.clone();
                        }
                        result.push(column);
                    }
                    if let Some(position) = excluded.iter().position(|e| !e) {
                        return Err(binder_error(format!(
                            "Column \"{}\" in EXCLUDE list not found in FROM clause",
                            star.exclude_list[position].column
                        )));
                    }
                    return Ok(result);
                }
                return Err(binder_error(format!(
                    "Referenced table \"{}\" not found!",
                    star.relation_name
                )));
            }
            if found.len() > 1 {
                return Err(BindContext::ambiguous_table(&star.relation_name));
            }
            found
        };
        let in_scope = |qualification: &[String], binding: &Binding| match qualification.last() {
            Some(table) => table.eq_ignore_ascii_case(&binding.alias),
            None => true,
        };
        // a USING column appears once, as `*` and not `t.*` would have it
        let mut handled_using_columns = Vec::new();
        let mut excluded = vec![false; star.exclude_list.len()];
        let mut replaced = vec![false; star.replace_list.len()];
        for binding in &bindings {
            for name in &binding.names {
                // CheckExclusionList
                let exclusion = star
                    .exclude_list
                    .iter()
                    .position(|e| e.column.eq_ignore_ascii_case(name) && in_scope(&e.qualification, binding));
                if let Some(position) = exclusion {
                    excluded[position] = true;
                    continue;
                }
                let set = if star.relation_name.is_empty() {
                    self.bind_context.using_binding_of(name, &binding.alias)
                } else {
                    None
                };
                let mut column: Expr = match set {
                    Some(set) if handled_using_columns.contains(&set) => continue,
                    Some(set) => {
                        handled_using_columns.push(set);
                        self.bind_context.using_columns[set].expression(name)
                    }
                    None => ExprKind::ColumnRef(self.bind_context.qualified_names(binding, name)).into(),
                };
                column.alias = name.clone();
                // HandleRename
                if let Some(position) = star.replace_list.iter().position(|(n, _)| n.eq_ignore_ascii_case(name)) {
                    if replaced[position] {
                        continue;
                    }
                    replaced[position] = true;
                    let (replaced_name, replacement) = &star.replace_list[position];
                    column = replacement.clone();
                    column.alias = replaced_name.clone();
                }
                let rename = star
                    .rename_list
                    .iter()
                    .position(|(n, _)| n.column.eq_ignore_ascii_case(name) && in_scope(&n.qualification, binding));
                if let Some(position) = rename {
                    column.alias = star.rename_list[position].1.clone();
                }
                result.push(column);
            }
        }
        let scope = if star.relation_name.is_empty() {
            "FROM clause"
        } else {
            star.relation_name.as_str()
        };
        let candidates = |missing: &str| {
            let mut unique: Vec<&String> = Vec::new();
            for name in bindings.iter().flat_map(|b| &b.names) {
                if !unique.iter().any(|u| u.eq_ignore_ascii_case(name)) {
                    unique.push(name);
                }
            }
            let scores = unique
                .into_iter()
                .map(|name| (name.clone(), similarity_rating(name, missing)))
                .collect();
            candidates_message(&top_n_strings(scores), "Candidate bindings")
        };
        if let Some((name, _)) = star.exclude_list.iter().zip(&excluded).find(|(_, found)| !**found) {
            let display: Vec<String> = name
                .qualification
                .iter()
                .chain([&name.column])
                .filter(|part| !part.is_empty())
                .map(|part| sql_quoted_identifier(part))
                .collect();
            return Err(binder_error(format!(
                "Column {} in EXCLUDE list not found in {scope}{}",
                display.join("."),
                candidates(&name.column)
            )));
        }
        if let Some(((name, _), _)) = star.replace_list.iter().zip(&replaced).find(|(_, found)| !**found) {
            return Err(binder_error(format!(
                "Column {} in REPLACE list not found in {scope}{}",
                sql_quoted_identifier(name),
                candidates(name)
            )));
        }
        Ok(result)
    }

    /// Applies a table reference's column aliases (`t(a, b)`) to its column names.
    pub(super) fn alias_columns(table_ref: &TableRef, alias: &str, mut names: Vec<String>) -> Result<Vec<String>> {
        if table_ref.column_name_alias.len() > names.len() {
            return Err(binder_error(format!(
                "table {} has {} columns available but {} columns specified",
                sql_quoted_identifier(alias),
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
    pub(super) fn bind_table_ref(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
        if table_ref.column_name_alias.iter().any(String::is_empty) {
            return Err(binder_error("column alias cannot be empty"));
        }
        let plan = self.bind_unsampled_table_ref(table_ref)?;
        match &table_ref.sample {
            Some(options) => bind_sample(options, plan),
            None => Ok(plan),
        }
    }

    fn bind_unsampled_table_ref(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
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
                // `ReplacementScan`: a file named like a table, once the catalog has no such table
                let lower = name.name.to_lowercase();
                if name.qualification.is_empty()
                    && [".csv", ".tsv", ".csv.gz", ".tsv.gz", ".csv.zst", ".tsv.zst"]
                        .iter()
                        .any(|end| lower.ends_with(end))
                    && !crate::execution::file_system::glob_or_directory(&name.name, "csv").is_empty()
                {
                    let call = ast::FunctionExpr::new(
                        "read_csv_auto",
                        vec![Expr::constant(ast::Literal::string(name.name.clone()))],
                    );
                    let mut file_ref = table_ref.clone();
                    if file_ref.alias.is_empty() {
                        let file = name.name.rsplit('/').next().unwrap_or(&name.name);
                        file_ref.alias = file.split('.').next().unwrap_or(file).to_string();
                    }
                    return self.bind_read_csv(&call, &file_ref);
                }
                if name.qualification.is_empty()
                    && [
                        ".json",
                        ".jsonl",
                        ".ndjson",
                        ".json.gz",
                        ".jsonl.gz",
                        ".ndjson.gz",
                        ".json.zst",
                        ".jsonl.zst",
                        ".ndjson.zst",
                    ]
                    .iter()
                    .any(|end| lower.ends_with(end))
                    && !crate::execution::file_system::glob_or_directory(&name.name, "json").is_empty()
                {
                    let call = ast::FunctionExpr::new(
                        "read_json_auto",
                        vec![Expr::constant(ast::Literal::string(name.name.clone()))],
                    );
                    let mut file_ref = table_ref.clone();
                    if file_ref.alias.is_empty() {
                        let file = name.name.rsplit('/').next().unwrap_or(&name.name);
                        file_ref.alias = file.split('.').next().unwrap_or(file).to_string();
                    }
                    return self.bind_read_json(&call, &file_ref);
                }
                if name.qualification.is_empty()
                    && lower.ends_with(".parquet")
                    && !crate::execution::file_system::glob_or_directory(&name.name, "parquet").is_empty()
                {
                    let call = ast::FunctionExpr::new(
                        "read_parquet",
                        vec![Expr::constant(ast::Literal::string(name.name.clone()))],
                    );
                    let mut file_ref = table_ref.clone();
                    if file_ref.alias.is_empty() {
                        let file = name.name.rsplit('/').next().unwrap_or(&name.name);
                        file_ref.alias = file.split('.').next().unwrap_or(file).to_string();
                    }
                    return self.bind_read_parquet(&call, &file_ref);
                }
                let recurring = matches!(name.qualification.as_slice(), [q] if q.eq_ignore_ascii_case("recurring"));
                if (name.qualification.is_empty() || recurring)
                    && let Some(working) = self
                        .working_tables
                        .iter()
                        .rev()
                        .filter(|w| !(w.transition && self.ctes.iter().any(|(n, ..)| n.eq_ignore_ascii_case(&w.name))))
                        .find(|w| w.name.eq_ignore_ascii_case(&name.name))
                {
                    let mut working = working.clone();
                    working.used.store(true, Ordering::Relaxed);
                    if recurring {
                        working.rows = working.recurring.clone();
                    }
                    let alias = if table_ref.alias.is_empty() {
                        working.name.clone()
                    } else {
                        table_ref.alias.clone()
                    };
                    let names = Self::alias_columns(table_ref, &alias, working.names)?;
                    let table_index = self.generate_table_index();
                    self.bind_context.add_binding(Binding {
                        alias,
                        schema: None,
                        table_index,
                        names,
                        types: working.types.clone(),
                        row_id: None,
                        virtual_columns: Vec::new(),
                        generated: Vec::new(),
                    });
                    return Ok(LogicalOperator::new(
                        LogicalOperatorKind::WorkingTable {
                            table_index,
                            types: working.types,
                            working: working.rows,
                        },
                        vec![],
                    ));
                }
                if name.qualification.is_empty()
                    && let Some(index) = self.ctes.iter().rposition(|(n, ..)| n.eq_ignore_ascii_case(&name.name))
                {
                    return self.bind_cte_reference(table_ref, index);
                }
                let entry = self.catalog.get_entry(
                    self.schema_of(name)?.as_deref(),
                    self.search_schema.as_deref(),
                    &name.name,
                );
                if entry.is_err()
                    && name.qualification.is_empty()
                    && self
                        .ctes_being_bound
                        .iter()
                        .any(|cte| cte.eq_ignore_ascii_case(&name.name))
                {
                    return Err(binder_error(format!(
                        "Circular reference to CTE {}, use WITH RECURSIVE to use recursive CTEs.",
                        name.name
                    )));
                }
                if entry.is_err() && name.qualification.is_empty() && Self::is_system_table(&name.name) {
                    return self.bind_system_table(table_ref, &name.name);
                }
                // `BindBaseTableRef`: a file that is there, but that no reader is for
                if entry.is_err()
                    && name.qualification.is_empty()
                    && name.name.contains(['.', '/', '\\'])
                    && std::path::Path::new(&name.name).is_file()
                {
                    return Err(binder_error(format!(
                        "No extension found that is capable of reading the file \"{}\"\n* If this file is a \
                         supported file format you can explicitly use the reader functions, such as read_csv, \
                         read_json or read_parquet",
                        name.name
                    )));
                }
                // the `duckdb_logs` view of `default_views.cpp`
                if entry.is_err() && name.qualification.is_empty() && name.name.eq_ignore_ascii_case("duckdb_logs") {
                    return self.bind_log_table("duckdb_logs", true, table_ref);
                }
                // the other views of the system catalog (`DefaultViewGenerator`), by their query
                if entry.is_err() {
                    let parts: Vec<&String> = name.qualification.iter().filter(|q| !q.is_empty()).collect();
                    let schema = match parts.as_slice() {
                        [] => Some(None),
                        [schema] => Some(Some(schema.as_str())),
                        [catalog, schema] if catalog.eq_ignore_ascii_case("system") => Some(Some(schema.as_str())),
                        _ => None,
                    };
                    if let Some(schema) = schema
                        && let Some(sql) = crate::catalog::defaults::default_view(schema, &name.name)
                    {
                        let select = super::table_functions::parse_select(sql, "Expected a single SELECT statement")?;
                        let mut table_ref = table_ref.clone();
                        if table_ref.alias.is_empty() {
                            table_ref.alias = name.name.clone();
                        }
                        return self.bind_rewritten(&select, &table_ref, &name.name);
                    }
                }
                let table = match entry? {
                    TableOrView::Table(table) => table,
                    TableOrView::View(view) => return self.bind_view(table_ref, view),
                };
                let alias = if table_ref.alias.is_empty() {
                    table.name.clone()
                } else {
                    table_ref.alias.clone()
                };
                let names = table.logical_columns().iter().map(|c| c.name.to_string()).collect();
                let names = Self::alias_columns(table_ref, &alias, names)?;
                let table_index = self.generate_table_index();
                let mut binding = table_binding(&table, alias, table_index, names);
                binding.schema = table_ref.alias.is_empty().then(|| table.schema.clone());
                self.bind_context.add_binding(binding);
                // the scan also outputs the rowid, past the columns
                let column_ids = (0..table.columns.len()).chain([COLUMN_IDENTIFIER_ROW_ID]).collect();
                Ok(LogicalOperator::new(
                    LogicalOperatorKind::Get {
                        table,
                        table_index,
                        column_ids,
                        filters: Vec::new(),
                        projection: None,
                        index_rows: None,
                    },
                    vec![],
                ))
            }
            TableRefKind::ExpressionList(rows) => {
                let (types, expressions) = self.bind_values(rows, None, &[])?;
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
                    schema: None,
                    table_index,
                    names,
                    types: types.clone(),
                    row_id: None,
                    virtual_columns: Vec::new(),
                    generated: Vec::new(),
                });
                Ok(LogicalOperator::new(
                    LogicalOperatorKind::ExpressionGet {
                        table_index,
                        types,
                        expressions,
                    },
                    vec![],
                ))
            }
            TableRefKind::Join(join) => self.bind_join(table_ref, join),
            TableRefKind::Subquery(select) => {
                // `Binder::Bind(SubqueryRef)`
                let mut binder = self.child_binder();
                binder.left_of_join = std::mem::take(&mut self.left_of_join);
                let bound = binder.bind_select_statement(select)?;
                self.left_of_join = binder.left_of_join;
                self.next_table_index = binder.next_table_index;
                let alias = if table_ref.alias.is_empty() {
                    let index = self.unnamed_subquery_index;
                    self.unnamed_subquery_index += 1;
                    match index {
                        1 => "unnamed_subquery".to_string(),
                        index => format!("unnamed_subquery{index}"),
                    }
                } else {
                    table_ref.alias.clone()
                };
                self.add_query_binding(alias, bound, &table_ref.column_name_alias)
            }
            TableRefKind::TableFunction {
                function,
                with_ordinality,
            } => {
                if *with_ordinality {
                    return Err(not_implemented("WITH ORDINALITY"));
                }
                if let ExprKind::Function(call) = &function.kind
                    && let Some(entry) = self.find_macro(&call.name, true)?
                {
                    return self.bind_table_macro(table_ref, call, entry);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(
                        call.name.name.to_lowercase().as_str(),
                        "read_single_csv_file" | "read_single_parquet_file"
                    )
                {
                    return self.bind_read_single_file(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(call.name.name.to_lowercase().as_str(), "read_csv" | "read_csv_auto")
                {
                    return self.bind_read_csv(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(
                        call.name.name.to_lowercase().as_str(),
                        "read_json"
                            | "read_json_auto"
                            | "read_ndjson"
                            | "read_ndjson_auto"
                            | "read_json_objects"
                            | "read_json_objects_auto"
                            | "read_ndjson_objects"
                    )
                {
                    return self.bind_read_json(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(call.name.name.to_lowercase().as_str(), "read_parquet" | "parquet_scan")
                {
                    return self.bind_read_parquet(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(
                        call.name.name.to_lowercase().as_str(),
                        "parquet_metadata"
                            | "parquet_schema"
                            | "parquet_kv_metadata"
                            | "parquet_file_metadata"
                            | "parquet_bloom_probe"
                            | "parquet_page_index"
                    )
                {
                    return self.bind_parquet_metadata(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(call.name.name.to_lowercase().as_str(), "json_each" | "json_tree")
                {
                    return self.bind_json_table_in_out(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(call.name.name.to_lowercase().as_str(), "read_text" | "read_blob")
                {
                    return self.bind_read_file(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && call.name.name.eq_ignore_ascii_case("sniff_csv")
                {
                    return self.bind_sniff_csv(call, table_ref);
                }
                if let ExprKind::Function(call) = &function.kind
                    && let Some(plan) = self.bind_rewritten_function(function, call, table_ref)?
                {
                    return Ok(plan);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && matches!(
                        call.name.name.to_lowercase().as_str(),
                        "pragma_table_info" | "pragma_show"
                    )
                    && let [argument] = call.args.as_slice()
                {
                    let value = self.constant_argument(&argument.expr)?;
                    let text = value.try_to_string()?;
                    let mut parts: Vec<String> = text.split('.').map(str::to_string).collect();
                    let table = QualifiedName {
                        name: parts.pop().unwrap_or_default(),
                        qualification: parts,
                    };
                    return self.bind_pragma_table_info(table_ref, &call.name.name, &table);
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && call.args.is_empty()
                    && Self::is_system_table(&call.name.name)
                {
                    return self.bind_system_table(table_ref, &call.name.name);
                }
                self.bind_range(function, table_ref)
            }
            TableRefKind::Pivot(pivot) => self.bind_pivot(table_ref, pivot),
            TableRefKind::Show(show) => self.bind_show_ref(table_ref, show),
            _ => Err(not_implemented("this kind of table reference")),
        }
    }

    /// `BindContext::AddSubquery`: makes the columns of a bound query visible under `alias`.
    pub(super) fn add_query_binding(
        &mut self,
        alias: String,
        bound: BoundStatement,
        column_aliases: &[String],
    ) -> Result<LogicalOperator> {
        let names = alias_column_names(&alias, &bound.names, column_aliases)?;
        let table_index = bound.plan.column_bindings()[0].table_index;
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            table_index,
            names,
            types: bound.types,
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        Ok(bound.plan)
    }

    /// The view branch of `Binder::Bind(BaseTableRef)`: a view is a subquery with the view's
    /// query, bound on its own, so that it sees nothing of this query, and looking in the view's
    /// schema first.
    fn bind_view(&mut self, table_ref: &TableRef, view: Arc<ViewCatalogEntry>) -> Result<LogicalOperator> {
        if self.bound_views.iter().any(|bound| Arc::ptr_eq(bound, &view)) {
            return Err(binder_error(format!(
                "infinite recursion detected: attempting to recursively bind view {}",
                sql_quoted_identifier(&view.name)
            )));
        }
        // the aliases of the reference override the ones of the view
        let mut column_aliases = view.aliases.clone();
        for (i, alias) in table_ref.column_name_alias.iter().enumerate() {
            match column_aliases.get_mut(i) {
                Some(column) => column.clone_from(alias),
                None => column_aliases.push(alias.clone()),
            }
        }
        let mut binder = self.child_binder();
        binder.ctes.clear();
        binder.outer_contexts.clear();
        binder.bound_views.push(view.clone());
        binder.search_schema = Some(view.schema.clone());
        let bound = binder.bind_select_statement(&view.query)?;
        self.next_table_index = binder.next_table_index;
        let alias = if table_ref.alias.is_empty() {
            view.name.clone()
        } else {
            table_ref.alias.clone()
        };
        let schema = table_ref.alias.is_empty().then(|| view.schema.clone());
        let plan = self.add_query_binding(alias, bound, &column_aliases)?;
        self.bind_context.bindings.last_mut().expect("just added").schema = schema;
        Ok(plan)
    }

    /// The `range` and `generate_series` table functions over constant integers.
    fn bind_range(&mut self, function: &Expr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let ExprKind::Function(call) = &function.kind else {
            return Err(not_implemented("table functions"));
        };
        let name = call.name.name.to_lowercase();
        if name == "duckdb_settings" {
            return self.bind_duckdb_settings(call, table_ref);
        }
        if name == "duckdb_logs" || name == "duckdb_log_contexts" {
            return self.bind_duckdb_logs(&name, call, table_ref);
        }
        if name == "pragma_storage_info" {
            return self.bind_storage_info(call, table_ref);
        }
        let generate_series = name == "generate_series";
        if !(generate_series || name == "range") || !call.name.qualification.is_empty() {
            // the table macros of the system catalog (`DefaultTableFunctionGenerator`)
            let schema = call
                .name
                .qualification
                .iter()
                .rfind(|q| !q.is_empty())
                .map(String::as_str);
            if let Some(entry) = crate::catalog::defaults::default_macro(schema, &call.name.name, true) {
                return self.bind_table_macro(table_ref, call, entry);
            }
            return Err(not_implemented(&format!("the table function {name}")));
        }
        if call.args.is_empty() || call.args.len() > 3 || call.args.iter().any(|a| !a.name.is_empty()) {
            return Err(not_implemented("this call of a range function"));
        }
        let mut values = Vec::with_capacity(call.args.len());
        for argument in &call.args {
            let mut binder = self.child_binder();
            binder.left_of_join = std::mem::take(&mut self.left_of_join);
            let bound = binder.bind_expression(&argument.expr);
            self.left_of_join = binder.left_of_join;
            let bound = bound?;
            let integral = bound.return_type.is_integral() || bound.return_type.id() == LogicalTypeId::SqlNull;
            if !bound.is_foldable() || !integral {
                // what depends on the row around it, and dates: a list per row that unnests
                let select = parse_ast(&format!("SELECT unnest({function}) AS {name}"))?.remove(0);
                let ast::Statement::Select(select) = select else {
                    unreachable!("this text is a SELECT");
                };
                return self.bind_rewritten(&select, table_ref, &name);
            }
            let value = evaluate_scalar(&bound.cast_to(&LogicalType::BIGINT, false))?;
            values.push(value.as_i128());
        }
        // any NULL argument makes the range empty
        let (start, count, increment) = match values.iter().copied().collect::<Option<Vec<i128>>>() {
            Some(values) => range_parameters(&values, generate_series)?,
            None => (0, 0, 1),
        };
        let alias = if table_ref.alias.is_empty() {
            name.clone()
        } else {
            table_ref.alias.clone()
        };
        let names = Self::alias_columns(table_ref, &alias, vec![name])?;
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            table_index,
            names,
            types: vec![LogicalType::BIGINT],
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        Ok(LogicalOperator::new(
            LogicalOperatorKind::Range {
                table_index,
                start,
                increment,
                count,
            },
            vec![],
        ))
    }

    /// `Binder::Bind(ExpressionListRef)`: binds the rows of a VALUES list. With
    /// `expected_types`, every value is cast to its column's type; without, each column gets
    /// the type all its values can be cast to.
    fn bind_values(
        &mut self,
        rows: &[Vec<Expr>],
        expected_types: Option<&[LogicalType]>,
        defaults: &[Option<BoundExpr>],
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
                    (ExprKind::Default, Some(types)) => defaults
                        .get(i)
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| BoundExpr::constant(Value::null(types[i].clone()))),
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
    /// Makes the CTEs of a statement visible, for what `bind` binds.
    pub(super) fn with_ctes<T>(&mut self, cte_map: &CteMap, bind: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let scope = self.ctes.len();
        for (name, cte) in &cte_map.0 {
            self.ctes
                .push((name.clone(), Arc::new(cte.clone()), self.outer_contexts.len()));
        }
        let result = bind(self);
        self.ctes.truncate(scope);
        result
    }

    /// The target of an UPDATE or DELETE, with what it binds to: only a base table will do.
    pub(super) fn bind_write_target(
        &mut self,
        table_ref: &TableRef,
        error: &str,
    ) -> Result<(LogicalOperator, Arc<TableCatalogEntry>, ColumnBinding)> {
        if !matches!(table_ref.kind, TableRefKind::BaseTable { .. }) {
            return Err(binder_error(error));
        }
        let plan = self.bind_table_ref(table_ref)?;
        let LogicalOperatorKind::Get { table, table_index, .. } = &plan.kind else {
            return Err(binder_error(error));
        };
        let table = table.clone();
        let row_id = ColumnBinding::new(*table_index, table.columns.len());
        Ok((plan, table, row_id))
    }

    /// `Binder::BindReturning`: the expressions over the rows a statement changed.
    pub(super) fn bind_returning(
        &mut self,
        plan: LogicalOperator,
        returning: &[Expr],
        table: &TableCatalogEntry,
        table_index: usize,
        alias: String,
    ) -> Result<BoundStatement> {
        self.bind_context = BindContext::default();
        // the generated columns too, computed from the stored ones the statement returns (not with
        // MERGE, whose `merge_action` comes after the stored columns)
        let logical = !table.generated.is_empty() && !self.merge_action_column;
        if logical {
            let names = table.logical_columns().iter().map(|c| c.name.to_string()).collect();
            let mut binding = super::generated::table_binding(table, alias, table_index, names);
            binding.schema = Some(table.schema.clone());
            binding.row_id = None;
            self.bind_context.add_binding(binding);
        } else {
            self.bind_context.add_binding(Binding {
                alias,
                // `schema.table.column` reads it too
                schema: Some(table.schema.clone()),
                table_index,
                names: table.columns.iter().map(|c| c.name.clone()).collect(),
                types: table.types(),
                row_id: None,
                virtual_columns: Vec::new(),
                generated: Vec::new(),
            });
        }
        let excluded_star =
            |e: &Expr| matches!(&e.kind, ExprKind::Star(s) if s.relation_name.eq_ignore_ascii_case("excluded"));
        if returning.iter().any(excluded_star) {
            return Err(excluded_in_returning());
        }
        let list = self.expand_star_expressions(returning)?;
        // `VerifyNotExcluded`
        for expr in &list {
            let mut excluded = false;
            let mut subquery = false;
            visit_expr_mut(&mut expr.clone(), &mut |e| {
                subquery |= matches!(e.kind, ExprKind::Subquery { .. });
                if let ExprKind::ColumnRef(names) = &e.kind
                    && names.len() > 1
                    && names[names.len() - 2].eq_ignore_ascii_case("excluded")
                {
                    excluded = true;
                }
            });
            if excluded {
                return Err(excluded_in_returning());
            }
            // `ReturningBinder::BindExpression`
            if subquery {
                return Err(binder_error("SUBQUERY is not supported in returning statements"));
            }
        }
        let names: Vec<String> = list.iter().map(expression_name).collect();
        if self.merge_action_column {
            // a virtual column: `*` left it out
            let columns = table.columns.len();
            let mut hidden: Vec<String> = (0..columns).map(|i| format!("\u{1}{i}")).collect();
            hidden.push("merge_action".to_string());
            let mut types = vec![LogicalType::SQLNULL; columns];
            types.push(LogicalType::VARCHAR);
            self.bind_context.add_binding(Binding {
                alias: "\u{1}merge".to_string(),
                schema: None,
                table_index,
                names: hidden,
                types,
                row_id: None,
                virtual_columns: Vec::new(),
                generated: Vec::new(),
            });
        }
        let mut expressions = list
            .iter()
            .map(|e| self.bind_expression(e))
            .collect::<Result<Vec<_>>>()?;
        if logical {
            // a stored column by its position among the stored ones, which is what the statement returns
            let columns = table.logical_columns();
            let stored: Vec<Option<usize>> = columns
                .iter()
                .scan(0, |next, c| {
                    Some(c.generated.is_none().then(|| {
                        *next += 1;
                        *next - 1
                    }))
                })
                .collect();
            for e in &mut expressions {
                e.walk_mut(&mut |e| {
                    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                        && binding.table_index == table_index
                        && let Some(Some(position)) = stored.get(binding.column_index)
                    {
                        binding.column_index = *position;
                    }
                });
            }
        }
        let types: Vec<LogicalType> = expressions.iter().map(|e| e.return_type.clone()).collect();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index: self.generate_table_index(),
                expressions,
            },
            vec![plan],
        );
        let column_aliases = names.iter().cloned().zip(0..).collect();
        Ok(BoundStatement {
            plan,
            names,
            types,
            column_aliases,
            return_type: StatementReturnType::QueryResult,
        })
    }

    fn changed_rows(plan: LogicalOperator) -> BoundStatement {
        BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            column_aliases: Vec::new(),
            return_type: StatementReturnType::ChangedRows,
        }
    }

    /// The alias of the target of an UPDATE or DELETE.
    pub(super) fn write_alias(table_ref: &TableRef, table: &TableCatalogEntry) -> String {
        if table_ref.alias.is_empty() {
            table.name.clone()
        } else {
            table_ref.alias.clone()
        }
    }

    /// `Binder::Bind(DeleteStatement)`.
    pub(super) fn bind_delete(&mut self, delete: &DeleteStatement) -> Result<BoundStatement> {
        self.with_ctes(&delete.cte_map, |binder| {
            let (mut plan, table, row_id) =
                binder.bind_write_target(&delete.table, "Can only delete from base table")?;
            for using in &delete.using {
                let mut context = std::mem::take(&mut binder.bind_context);
                let right = binder.bind_table_ref(using)?;
                context.add_context(std::mem::take(&mut binder.bind_context));
                binder.bind_context = context;
                plan = LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![plan, right]);
            }
            if let Some(condition) = &delete.condition {
                let condition = binder.bind_expression(condition)?.cast_to(&LogicalType::BOOLEAN, false);
                plan = LogicalOperator::new(
                    LogicalOperatorKind::Filter {
                        expressions: vec![condition],
                    },
                    vec![plan],
                );
            }
            let projection = LogicalOperator::new(
                LogicalOperatorKind::Projection {
                    table_index: binder.generate_table_index(),
                    expressions: vec![BoundExpr::column_ref(row_id, LogicalType::BIGINT, "rowid")],
                },
                vec![plan],
            );
            let table_index = binder.generate_table_index();
            let returning = !delete.returning.is_empty();
            let plan = LogicalOperator::new(
                LogicalOperatorKind::Delete {
                    table: table.clone(),
                    table_index,
                    returning,
                },
                vec![projection],
            );
            if returning {
                let alias = Self::write_alias(&delete.table, &table);
                return binder.bind_returning(plan, &delete.returning, &table, table_index, alias);
            }
            Ok(Self::changed_rows(plan))
        })
    }

    /// `Binder::Bind(UpdateStatement)`.
    pub(super) fn bind_update(&mut self, update: &UpdateStatement) -> Result<BoundStatement> {
        self.with_ctes(&update.cte_map, |binder| {
            let (mut plan, table, row_id) = binder.bind_write_target(&update.table, "Can only update base table")?;
            // the defaults of a table of a nested schema are bound in that schema
            if table.schema.contains(crate::catalog::SCHEMA_PATH_SEPARATOR) && binder.search_schema.is_none() {
                binder.search_schema = Some(table.schema.clone());
            }
            if let Some(from) = &update.from_table {
                let mut context = std::mem::take(&mut binder.bind_context);
                let right = binder.bind_table_ref(from)?;
                context.add_context(std::mem::take(&mut binder.bind_context));
                binder.bind_context = context;
                plan = LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![plan, right]);
            }
            let info = &update.set_info;
            if let Some(condition) = &info.condition {
                let condition = binder.bind_expression(condition)?.cast_to(&LogicalType::BOOLEAN, false);
                plan = LogicalOperator::new(
                    LogicalOperatorKind::Filter {
                        expressions: vec![condition],
                    },
                    vec![plan],
                );
            }
            let mut columns = Vec::with_capacity(info.columns.len());
            let mut expressions = Vec::with_capacity(info.columns.len() + 1);
            for (name, value) in info.columns.iter().zip(&info.expressions) {
                if table.is_generated(name) {
                    return Err(binder_error(format!(
                        "Cant update column \"{name}\" because it is a generated column!"
                    )));
                }
                let Some(index) = table.column_index(name) else {
                    return Err(binder_error(format!(
                        "Referenced update column {name} not found in table!\n{}",
                        candidates_message(
                            &top_n_strings(
                                table
                                    .columns
                                    .iter()
                                    .map(|c| (c.name.clone(), similarity_rating(&c.name, name)))
                                    .collect()
                            ),
                            "Candidate bindings"
                        )
                    )));
                };
                if columns.contains(&index) {
                    return Err(binder_error(format!("Multiple assignments to same column \"{name}\"")));
                }
                columns.push(index);
                let column = &table.columns[index];
                let bound = if matches!(value.kind, ExprKind::Default) {
                    match &column.default_value {
                        Some(default) => default.clone(),
                        None => BoundExpr::constant(Value::null(column.ty.clone())),
                    }
                } else {
                    binder.bind_expression(value)?
                };
                expressions.push(checked_cast(bound, &column.ty)?);
            }
            expressions.push(BoundExpr::column_ref(row_id, LogicalType::BIGINT, "rowid"));
            let projection = LogicalOperator::new(
                LogicalOperatorKind::Projection {
                    table_index: binder.generate_table_index(),
                    expressions,
                },
                vec![plan],
            );
            let table_index = binder.generate_table_index();
            let returning = !update.returning.is_empty();
            let plan = LogicalOperator::new(
                LogicalOperatorKind::Update {
                    table: table.clone(),
                    table_index,
                    columns,
                    returning,
                },
                vec![projection],
            );
            if returning {
                let alias = Self::write_alias(&update.table, &table);
                return binder.bind_returning(plan, &update.returning, &table, table_index, alias);
            }
            Ok(Self::changed_rows(plan))
        })
    }

    /// `Binder::BindNode(InsertQueryNode)`.
    pub(super) fn bind_insert(&mut self, insert: &InsertStatement) -> Result<BoundStatement> {
        self.with_ctes(&insert.cte_map, |binder| binder.bind_insert_body(insert))
    }

    fn bind_insert_body(&mut self, insert: &InsertStatement) -> Result<BoundStatement> {
        let table = self
            .catalog
            .get_table(self.schema_of(&insert.table)?.as_deref(), &insert.table.name)?;
        // the defaults of a table of a nested schema are bound in that schema
        if table.schema.contains(crate::catalog::SCHEMA_PATH_SEPARATOR) && self.search_schema.is_none() {
            self.search_schema = Some(table.schema.clone());
        }
        if insert.by_name {
            // the columns are the names of the query
            let InsertValues::Select(select) = &insert.values else {
                return Err(binder_error(
                    "INSERT BY NAME cannot be combined with with DEFAULT VALUES",
                ));
            };
            if values_list(select).is_some() {
                return Err(binder_error(
                    "INSERT BY NAME can only be used when inserting from a SELECT statement",
                ));
            }
            if !insert.columns.is_empty() {
                return Err(binder_error(
                    "INSERT BY NAME cannot be combined with an explicit column list",
                ));
            }
            let mut binder = self.child_binder();
            let names = binder.bind_select_statement(select)?.names;
            let mut by_position = insert.clone();
            by_position.by_name = false;
            by_position.columns = names;
            return self.bind_insert_body(&by_position);
        }
        let on_conflict = insert
            .on_conflict
            .as_ref()
            .map(|info| self.bind_on_conflict(insert, info, &table))
            .transpose()?;
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
                    return Err(binder_error(format!(
                        "Duplicate column name {} in INSERT",
                        sql_quoted_identifier(name)
                    )));
                }
                if table.is_generated(name) {
                    return Err(binder_error("Cannot insert into a generated column"));
                }
                let Some(index) = table.column_index(name) else {
                    return Err(binder_error(format!(
                        "Table {} does not have a column with name {}",
                        sql_quoted_identifier(&table.name),
                        sql_quoted_identifier(name)
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
                    sql_quoted_identifier(&table.name)
                )
            } else {
                format!(
                    "table {} has {expected} columns but {supplied} values were supplied",
                    sql_quoted_identifier(&table.name)
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
                    let defaults: Vec<Option<BoundExpr>> = if insert.columns.is_empty() {
                        table.columns.iter().map(|c| c.default_value.clone()).collect()
                    } else {
                        insert
                            .columns
                            .iter()
                            .map(|name| {
                                table
                                    .column_index(name)
                                    .and_then(|i| table.columns[i].default_value.clone())
                            })
                            .collect()
                    };
                    let (types, expressions) = self.bind_values(rows, Some(&expected_types), &defaults)?;
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
                    let mut binder = self.child_binder();
                    let bound = binder.bind_select_statement(select)?;
                    self.next_table_index = binder.next_table_index;
                    if bound.types.len() != expected_types.len() {
                        return Err(mismatch(expected_types.len(), bound.types.len()));
                    }
                    // cast the query's columns to the column types
                    self.cast_logical_operator_to_types(&bound.types, &expected_types, bound.plan)?
                }
            },
        };
        let table_index = self.generate_table_index();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::Insert {
                table: table.clone(),
                column_index_map,
                table_index,
                returning: !insert.returning.is_empty(),
                on_conflict: on_conflict.map(Box::new),
            },
            vec![source],
        );
        if !insert.returning.is_empty() {
            let alias = insert.table_alias.clone().unwrap_or_else(|| table.name.clone());
            return self.bind_returning(plan, &insert.returning, &table, table_index, alias);
        }
        Ok(BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            column_aliases: Vec::new(),
            return_type: StatementReturnType::ChangedRows,
        })
    }

    /// `Binder::CastLogicalOperatorToTypes`: casts inside a projection at the root of the plan,
    /// or else pushes a new projection to hold the casts.
    fn cast_logical_operator_to_types(
        &mut self,
        source_types: &[LogicalType],
        target_types: &[LogicalType],
        mut op: LogicalOperator,
    ) -> Result<LogicalOperator> {
        for (source, target) in source_types.iter().zip(target_types) {
            check_cast(source, target)?;
        }
        if source_types == target_types {
            return Ok(op);
        }
        if let LogicalOperatorKind::Projection { expressions, .. } = &mut op.kind {
            for (expr, target) in expressions.iter_mut().zip(target_types) {
                *expr = std::mem::replace(expr, BoundExpr::constant(Value::sql_null())).cast_to(target, false);
            }
            return Ok(op);
        }
        // found a non-projection operator: push a new projection containing the casts
        let expressions = op
            .column_bindings()
            .into_iter()
            .zip(source_types.iter().zip(target_types))
            .map(|(binding, (source, target))| {
                BoundExpr::column_ref(binding, source.clone(), String::new()).cast_to(target, false)
            })
            .collect();
        let table_index = self.generate_table_index();
        Ok(LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index,
                expressions,
            },
            vec![op],
        ))
    }

    /// `Binder::BindCreateTableInfo`.
    /// Binds a table of a database file, whose types are known. `search_schema` is where its names
    /// are looked up, as C++ binds them in the schema of the table.
    pub fn bind_stored_table(&mut self, info: &CreateTableInfo, search_schema: &str) -> Result<LogicalOperator> {
        self.search_schema = Some(search_schema.to_string());
        self.bind_create_table(info).map(|bound| bound.plan)
    }

    pub(super) fn bind_create_table(&mut self, info: &CreateTableInfo) -> Result<BoundStatement> {
        if !info.options.is_empty() {
            return Err(Error::new(
                ErrorKind::Catalog,
                "WITH clause is not supported for tables in a duckdb catalog",
            ));
        }
        if !info.partition_keys.is_empty() || !info.sort_keys.is_empty() {
            return Err(not_implemented("table options"));
        }
        if !info.temporary
            && info
                .name
                .qualification
                .first()
                .is_some_and(|first| first.eq_ignore_ascii_case(TEMP_SCHEMA))
        {
            return Err(Error::new(
                ErrorKind::Parser,
                format!("Only TEMPORARY table names can use the \"{TEMP_SCHEMA}\" catalog"),
            ));
        }
        let schema_name = match info.temporary {
            true => Some(TEMP_SCHEMA.to_string()),
            false => self.schema_of(&info.name)?,
        };
        let schema = self.catalog.schema_for(schema_name.as_deref())?;
        // the names of a table of a nested schema are looked up in that schema
        if schema.path.len() > 1 && self.search_schema.is_none() {
            self.search_schema = Some(schema.ident());
        }
        let on_conflict = match info.on_conflict {
            AstConflict::Error => OnCreateConflict::Error,
            AstConflict::Ignore => OnCreateConflict::Ignore,
            AstConflict::Replace => OnCreateConflict::Replace,
        };
        if let Some(query) = &info.query {
            return self.bind_create_table_as(info, query, schema, on_conflict);
        }
        let mut columns = Vec::with_capacity(info.columns.len());
        for (position, column) in info.columns.iter().enumerate() {
            if info.columns[..position]
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(&column.name))
            {
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!(
                        "Column with name {} already exists!",
                        sql_quoted_identifier(&column.name)
                    ),
                ));
            }
            if column.generated.is_some() {
                continue;
            }
            let Some(ty) = &column.column_type else {
                return Err(not_implemented("columns without a type"));
            };
            let mut definition = ColumnDefinition::new(column.name.clone(), self.bind_type(ty)?);
            if let Some(default) = &column.default_value {
                let mut binder = Binder::new(self.catalog);
                binder.aggregate_error = "DEFAULT value cannot contain aggregates!";
                binder.search_schema = self.search_schema.clone();
                definition.default_value = Some(checked_cast(binder.bind_expression(default)?, &definition.ty)?);
                definition.default_text = Some(default.to_string());
            }
            columns.push(definition);
        }
        let generated = self.bind_generated_columns(&info.name.name, &info.columns, &columns)?;
        if columns.is_empty() {
            return Err(binder_error(
                "Creating a table without physical (non-generated) columns is not supported",
            ));
        }
        let physical = CreateTableInfo {
            columns: info.columns.iter().filter(|c| c.generated.is_none()).cloned().collect(),
            constraints: physical_constraints(info)?,
            ..info.clone()
        };
        let constraints = self.bind_table_constraints(&physical, &mut columns, &schema)?;
        if !info.temporary {
            for column in &columns {
                self.catalog.check_column_type(&column.ty)?;
            }
        }
        let plan = LogicalOperator::new(
            LogicalOperatorKind::CreateTable {
                schema,
                name: info.name.name.clone(),
                columns,
                generated,
                constraints,
                on_conflict,
            },
            vec![],
        );
        Ok(BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            column_aliases: Vec::new(),
            return_type: StatementReturnType::Nothing,
        })
    }

    /// CREATE TABLE ... AS: the columns come from the query, with the names listed if any.
    fn bind_create_table_as(
        &mut self,
        info: &CreateTableInfo,
        query: &SelectStatement,
        schema: Arc<SchemaCatalogEntry>,
        on_conflict: OnCreateConflict,
    ) -> Result<BoundStatement> {
        if !info.constraints.is_empty() {
            return Err(not_implemented("table constraints"));
        }
        // the conflict is checked before the query runs
        if let Some(existing) = schema.get_entry(&info.name.name) {
            match on_conflict {
                OnCreateConflict::Error => {
                    return Err(Error::new(
                        ErrorKind::Catalog,
                        format!(
                            "{} with name \"{}\" already exists!",
                            existing.type_name(),
                            info.name.name
                        ),
                    ));
                }
                OnCreateConflict::Ignore => {
                    let plan = LogicalOperator::new(
                        LogicalOperatorKind::CreateTable {
                            schema,
                            name: info.name.name.clone(),
                            columns: Vec::new(),
                            generated: Vec::new(),
                            constraints: Vec::new(),
                            on_conflict,
                        },
                        vec![],
                    );
                    return Ok(BoundStatement {
                        plan,
                        names: vec!["Count".to_string()],
                        types: vec![LogicalType::BIGINT],
                        column_aliases: Vec::new(),
                        return_type: StatementReturnType::ChangedRows,
                    });
                }
                OnCreateConflict::Replace => {}
            }
        }
        let mut binder = self.child_binder();
        let bound = binder.bind_select_statement(query)?;
        // what flattens subqueries later takes its table indexes from here
        self.next_table_index = binder.next_table_index;
        if info.columns.len() > bound.types.len() {
            return Err(binder_error("Target table has more column names than query result."));
        }
        let mut columns: Vec<ColumnDefinition> = Vec::with_capacity(bound.names.len());
        for (i, (name, ty)) in bound.names.iter().zip(&bound.types).enumerate() {
            let name = info.columns.get(i).map_or(name, |c| &c.name);
            if columns.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!("Column with name {} already exists!", sql_quoted_identifier(name)),
                ));
            }
            // a column of untyped NULLs becomes an INTEGER column
            let ty = if ty.id() == LogicalTypeId::SqlNull {
                LogicalType::INTEGER
            } else {
                ty.clone()
            };
            columns.push(ColumnDefinition::new(name.clone(), ty));
        }
        if !info.temporary {
            for column in &columns {
                self.catalog.check_column_type(&column.ty)?;
            }
        }
        let plan = LogicalOperator::new(
            LogicalOperatorKind::CreateTableAs {
                schema,
                name: info.name.name.clone(),
                columns,
                on_conflict,
            },
            vec![bound.plan],
        );
        Ok(BoundStatement {
            plan,
            names: vec!["Count".to_string()],
            types: vec![LogicalType::BIGINT],
            column_aliases: Vec::new(),
            return_type: StatementReturnType::ChangedRows,
        })
    }
}

/// Fails like DuckDB would for a cast that is not implemented here yet.
/// The USING KEY clause: the columns it names are keys, or aggregates of what they get.
fn bind_cte_key(targets: &[Expr], names: &[String]) -> Result<Vec<CteKeyColumn>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let position = |name: &str| -> Result<usize> {
        names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))
            .ok_or_else(|| binder_error(format!("Referenced column \"{name}\" not found in FROM clause!")))
    };
    let mut columns = vec![CteKeyColumn::Last; names.len()];
    let mut seen = vec![false; names.len()];
    let mut has_key = false;
    for target in targets {
        let (column, mode) = match &target.kind {
            ExprKind::ColumnRef(parts) => {
                has_key = true;
                (position(parts.last().expect("a name"))?, CteKeyColumn::Key)
            }
            ExprKind::Function(call) if call.args.len() == 1 => {
                let ExprKind::ColumnRef(parts) = &call.args[0].expr.kind else {
                    return Err(binder_error(
                        "In USING KEY, an aggregate must either have a column reference or an empty argument list",
                    ));
                };
                let mode = match call.name.name.to_lowercase().as_str() {
                    "min" => CteKeyColumn::Min,
                    "max" => CteKeyColumn::Max,
                    "first" | "arbitrary" | "any_value" => CteKeyColumn::First,
                    "last" => CteKeyColumn::Last,
                    other => return Err(not_implemented(&format!("the aggregate {other} in USING KEY"))),
                };
                (position(parts.last().expect("a name"))?, mode)
            }
            _ => {
                return Err(binder_error(format!(
                    "'{target}' can't be used in the USING KEY clause."
                )));
            }
        };
        if std::mem::replace(&mut seen[column], true) {
            return Err(binder_error(format!(
                "Column {} referenced multiple times in USING KEY clause",
                names[column]
            )));
        }
        columns[column] = mode;
    }
    if !has_key {
        return Err(binder_error("USING KEY clause requires at least one key column."));
    }
    Ok(columns)
}

/// `LogicalSample`, with the check of `PlanSample`.
fn bind_sample(options: &ast::SampleOptions, plan: LogicalOperator) -> Result<LogicalOperator> {
    if !options.is_percentage && options.method == ast::SampleMethod::Bernoulli {
        return Err(Error::new(
            ErrorKind::Parser,
            format!(
                "Sample method {} cannot be used with a discrete sample count, either switch to reservoir sampling \
                 or use a sample_size",
                options.method.name()
            ),
        ));
    }
    Ok(LogicalOperator::new(
        LogicalOperatorKind::Sample {
            options: options.clone(),
        },
        vec![plan],
    ))
}

fn check_cast(from: &LogicalType, to: &LogicalType) -> Result<()> {
    if crate::function::cast::has_cast(from, to) {
        Ok(())
    } else {
        Err(crate::unported(format!("casting {from} to {to}")))
    }
}
