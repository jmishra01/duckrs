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
    BoundOrderByNode, COLUMN_IDENTIFIER_ROW_ID, CteKey, LogicalOperator, LogicalOperatorKind,
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
pub(super) fn expression_name(expr: &Expr) -> String {
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
    /// with the collation a position of a set operation is sorted by
    Order(Vec<(OrderModifiers, usize, Option<String>)>),
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
pub(super) fn alias_column_names(table: &str, names: &[String], aliases: &[String]) -> Result<Vec<String>> {
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

/// The alias of a COLLATE that an ORDER BY position applies to its select list entry.
pub(super) const ORDER_POSITION_COLLATE: &str = "\u{1}order position";

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
        // `db.t`: the database's default schema, unless it has a schema of that name
        if let [first] = name.qualification.as_slice()
            && first.eq_ignore_ascii_case(database)
            && self.session.is_some()
            && self.catalog.get_schema(first).is_none()
        {
            return Ok(None);
        }
        schema_of(name)
    }

    /// A table or view of the connection's default database, when this binder binds in another one, in the
    /// schema (a nested one too) a name is qualified with.
    fn default_database_entry_in(&self, schema: Option<&str>, name: &str) -> Option<crate::catalog::TableOrView> {
        let session = self.session.as_ref()?;
        if session.default_database.eq_ignore_ascii_case(&session.database) {
            return None;
        }
        let catalog = (session.attached_catalog)(&session.default_database)?;
        if std::ptr::eq(Arc::as_ptr(&catalog), self.catalog) {
            return None;
        }
        catalog.get_entry(schema, None, name).ok()
    }

    /// The catalog of an attached database other than this binder's that `name` is qualified by, and
    /// the schema in it (`None` for its default one). The first part of `a.b` is a schema if this
    /// catalog has one, as in C++.
    pub(super) fn other_database_of(
        &self,
        name: &QualifiedName,
    ) -> Option<(Arc<crate::catalog::Catalog>, Option<String>)> {
        let first = name.qualification.first()?;
        let session = self.session.as_ref()?;
        if first.eq_ignore_ascii_case(&session.database)
            || first.eq_ignore_ascii_case(TEMP_SCHEMA)
            || first.eq_ignore_ascii_case("system")
        {
            return None;
        }
        if name.qualification.len() == 1 && self.catalog.get_schema(first).is_some() {
            return None;
        }
        let catalog = (session.attached_catalog)(first)?;
        if std::ptr::eq(Arc::as_ptr(&catalog), self.catalog) {
            return None;
        }
        let schema = match &name.qualification[1..] {
            [] => None,
            [schema] => Some(schema.clone()),
            path => Some(crate::catalog::schema_key(path)),
        };
        Some((catalog, schema))
    }

    /// `Binder::Bind(SelectStatement)`.
    pub(super) fn bind_select_statement(&mut self, select: &SelectStatement) -> Result<BoundStatement> {
        self.bind_query_node(&select.node)
    }

    /// `Binder::BindNode(QueryNode)`: the CTEs of a query are visible for what the query binds.
    /// A CTE of a SELECT or set operation is planned once, as a `MaterializedCte` around the query
    /// (`FinishCTE`), whatever its materialization hint; one that reads the queries around it is
    /// bound again at each reference.
    pub(super) fn bind_query_node(&mut self, node: &QueryNode) -> Result<BoundStatement> {
        self.bind_query_node_with(node, |binder| match &node.kind {
            QueryNodeKind::Select(select) => binder.bind_select_node(select, &node.modifiers),
            QueryNodeKind::SetOperation(operation) => binder.bind_set_operation(operation, &node.modifiers),
            _ => Err(not_implemented("this kind of query")),
        })
    }

    /// [`Self::bind_query_node`], with `body` binding what the CTEs of `node` are visible to.
    fn bind_query_node_with(
        &mut self,
        node: &QueryNode,
        body: impl FnOnce(&mut Self) -> Result<BoundStatement>,
    ) -> Result<BoundStatement> {
        let scope = self.ctes.len();
        for (name, cte) in &node.cte_map.0 {
            match &cte.query.kind {
                QueryNodeKind::Select(_) | QueryNodeKind::SetOperation(_) => {}
                QueryNodeKind::RecursiveCte(_) => {}
                // the client runs those of the top-level query before it (`dml_cte`)
                QueryNodeKind::Copy(_) => {
                    return Err(binder_error(
                        "WITH clause containing a COPY statement must be at the top level",
                    ));
                }
                _ => {
                    return Err(binder_error(
                        "WITH clause containing a data-modifying statement must be at the top level",
                    ));
                }
            }
            // `PrepareCTE`: a CTE runs once, and `CTEInlining` puts its query back where that is
            // better, unless it says MATERIALIZED. A NOT MATERIALIZED one is planned the same way, and
            // `CTEInlining` inlines it wherever it can (and not at all when it is disabled).
            let materialized = matches!(
                cte.query.kind,
                QueryNodeKind::Select(_) | QueryNodeKind::SetOperation(_)
            )
            .then(|| {
                Arc::new(Mutex::new(super::MaterializedCteState {
                    cte_index: self.generate_table_index(),
                    data: Arc::new(Mutex::new(ColumnDataCollection::new(Vec::new()))),
                    bound: None,
                    inline: false,
                    always: cte.materialized == ast::CteMaterialize::Always,
                    never: cte.materialized == ast::CteMaterialize::Never,
                }))
            });
            self.ctes.push((
                name.clone(),
                Arc::new(cte.clone()),
                self.outer_contexts.len(),
                materialized,
            ));
        }
        let result = body(self);
        let pushed = self.ctes.split_off(scope);
        let mut result = result?;
        // `FinishCTE`: a materialized CTE that was referenced runs before the query, the first one
        // outermost
        for (name, _, _, state) in pushed.into_iter().rev() {
            let Some(state) = state else {
                continue;
            };
            let mut state = state.lock();
            if state.inline {
                continue;
            }
            let Some((plan, ..)) = state.bound.take() else {
                continue;
            };
            let query = std::mem::replace(
                &mut result.plan,
                LogicalOperator::new(LogicalOperatorKind::CrossProduct, Vec::new()),
            );
            result.plan = LogicalOperator::new(
                LogicalOperatorKind::MaterializedCte {
                    name,
                    table_index: state.cte_index,
                    data: state.data.clone(),
                    always: state.always,
                    never: state.never,
                },
                vec![plan, query],
            );
        }
        Ok(result)
    }

    /// A reference to the CTE at `index` of the visible ones (`Binder::Bind(BaseTableRef)`).
    fn bind_cte_reference(&mut self, table_ref: &TableRef, index: usize) -> Result<LogicalOperator> {
        let (name, cte, outer_depth, materialized) = self.ctes[index].clone();
        if cte.aliases.iter().any(String::is_empty) {
            return Err(binder_error("column alias cannot be empty"));
        }
        // C++ flattens subqueries before `CTEInlining`, which then inlines a NOT MATERIALIZED CTE
        // read in one; that pass here does not reach into subqueries, so such a reference binds
        // the definition itself when the pass is on
        let inline_here = materialized.as_ref().is_some_and(|state| state.lock().never)
            && self.options.optimizer.cte_inlining
            && self.outer_contexts.len() > outer_depth;
        if let Some(state) = &materialized
            && !inline_here
            && !state.lock().inline
        {
            let bound_already = state.lock().bound.is_some();
            if !bound_already {
                let before = self.correlated.lock().len();
                let bound = self.bind_cte_definition(&name, &cte, index, outer_depth)?;
                // a CTE defined in a subquery can read the queries around it
                let correlated = self.correlated.lock().len() != before || outer_depth > 0;
                let mut state = state.lock();
                if correlated {
                    state.inline = true;
                    drop(state);
                    return self.finish_cte_reference(table_ref, name, &cte, bound);
                }
                state.data = Arc::new(Mutex::new(ColumnDataCollection::new(bound.types.clone())));
                state.bound = Some((
                    bound.plan,
                    bound.names,
                    bound.types,
                    bound.column_aliases,
                    bound.return_type,
                ));
            }
            let state = state.lock();
            let (_, names, types, column_aliases, return_type) = state.bound.as_ref().expect("bound above");
            let reference = LogicalOperator::new(
                LogicalOperatorKind::CteRef {
                    table_index: self.generate_table_index(),
                    cte_index: state.cte_index,
                    types: types.clone(),
                    names: names.clone(),
                    data: state.data.clone(),
                },
                Vec::new(),
            );
            let bound = BoundStatement {
                plan: reference,
                names: names.clone(),
                types: types.clone(),
                column_aliases: column_aliases.clone(),
                return_type: *return_type,
            };
            drop(state);
            return self.finish_cte_reference(table_ref, name, &cte, bound);
        }
        let bound = self.bind_cte_definition(&name, &cte, index, outer_depth)?;
        self.finish_cte_reference(table_ref, name, &cte, bound)
    }

    /// The query of the CTE at `index`, bound as at its definition.
    fn bind_cte_definition(
        &mut self,
        name: &str,
        cte: &ast::CteInfo,
        index: usize,
        outer_depth: usize,
    ) -> Result<BoundStatement> {
        let name = name.to_string();
        let mut binder = self.child_binder();
        // a CTE sees the ones before it, and not itself
        binder.ctes.truncate(index);
        binder.ctes_being_bound.push(name.clone());
        // a CTE sees what is around its definition, and not what is around a reference to it
        binder.outer_contexts.truncate(outer_depth);
        let bound = match &cte.query.kind {
            // the WITH of the recursive CTE's own query is visible to both its parts
            QueryNodeKind::RecursiveCte(node) => {
                binder.bind_query_node_with(&cte.query, |b| b.bind_recursive_cte(&name, node, &cte.aliases))?
            }
            _ => binder.bind_query_node(&cte.query)?,
        };
        self.next_table_index = binder.next_table_index;
        Ok(bound)
    }

    fn finish_cte_reference(
        &mut self,
        table_ref: &TableRef,
        name: String,
        cte: &ast::CteInfo,
        bound: BoundStatement,
    ) -> Result<LogicalOperator> {
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

    /// What this binder evaluates while binding runs before any CTE has: its references bind the
    /// CTE's query in place.
    fn inline_ctes(&mut self) {
        for cte in &mut self.ctes {
            cte.3 = None;
        }
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
        let (key, result_types) = self.bind_cte_key(name, &node.key_targets, &names, &types)?;
        // with USING KEY and UNION, the recursive part reads the finalized rows of the keys that
        // changed, and `recurring.name` reads those of all keys
        let recursive_types = if !key.is_empty() && !node.union_all {
            result_types.clone()
        } else {
            types.clone()
        };
        let working = Arc::new(Mutex::new(ColumnDataCollection::new(recursive_types.clone())));
        let recurring = Arc::new(Mutex::new(ColumnDataCollection::new(result_types.clone())));
        let used = Arc::new(AtomicBool::new(false));
        let mut binder = self.child_binder();
        binder.working_tables.push(WorkingTable {
            name: name.to_string(),
            names: names.clone(),
            types: recursive_types,
            recurring_types: result_types.clone(),
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
        let mut key = key;
        key.once = !key.is_empty() && !used.load(Ordering::Relaxed);
        let plan = if used.load(Ordering::Relaxed) || !key.is_empty() {
            // `recursive_cte_tables` has the CTE while its recursive part is planned
            let mut options = self.options.optimizer;
            options.range_joins.in_recursive_cte = true;
            let mut recursive = optimize(recursive, options);
            let unresolved = Box::new(recursive.clone());
            resolve_column_bindings(&mut recursive)?;
            LogicalOperator::new(
                LogicalOperatorKind::RecursiveCte {
                    name: name.to_string(),
                    table_index: self.generate_table_index(),
                    types: result_types.clone(),
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
            types: result_types,
            column_aliases,
            return_type: StatementReturnType::QueryResult,
        })
    }

    /// `Binder::BindNode(RecursiveCTENode)`: the USING KEY clause. The columns it names are the
    /// keys; each other column is given by an aggregate of the rows of its key, the one USING KEY
    /// calls on it or else `last`. Returns the key, and the types of the CTE, which are those of
    /// the aggregates.
    fn bind_cte_key(
        &mut self,
        name: &str,
        targets: &[Expr],
        names: &[String],
        types: &[LogicalType],
    ) -> Result<(CteKey, Vec<LogicalType>)> {
        if targets.is_empty() {
            return Ok((CteKey::default(), types.to_vec()));
        }
        let cant_be_used = |target: &Expr| {
            binder_error(format!(
                "'{target}' can't be used in the USING KEY clause.\nIt has to be either a column name as a key or a \
                 direct call to an aggregate function."
            ))
        };
        let subquery_error = || binder_error("Subqueries are not yet supported for aggregates in USING KEY");
        // the aggregates read the rows of the CTE, and nothing around it
        let mut binder = self.child_binder();
        binder.outer_contexts.clear();
        binder.outer_aliases.clear();
        binder.working_tables.clear();
        binder.correlated = Arc::default();
        let table_index = binder.generate_table_index();
        binder.bind_context.add_binding(Binding {
            alias: name.to_string(),
            schema: None,
            catalog: None,
            table_index,
            names: names.to_vec(),
            types: types.to_vec(),
            row_id: None,
            virtual_columns: Vec::new(),
            generated: Vec::new(),
        });
        let column_of = |bound: &BoundExpr| match &bound.kind {
            BoundExprKind::ColumnRef { binding, .. } if binding.table_index == table_index => {
                Some(binding.column_index)
            }
            _ => None,
        };
        let mut keys: Vec<usize> = Vec::new();
        let mut payload: Vec<(usize, crate::planner::expression::BoundAggregate)> = Vec::new();
        let mut return_types = types.to_vec();
        for target in targets {
            match &target.kind {
                ExprKind::ColumnRef(_) => {
                    if !target.alias.is_empty() {
                        return Err(binder_error(
                            "In USING KEY, only direct calls to an aggregate function can have an alias.",
                        ));
                    }
                    let bound = binder.bind_expression(target)?;
                    let column = column_of(&bound).ok_or_else(|| cant_be_used(target))?;
                    if !keys.contains(&column) {
                        keys.push(column);
                    }
                }
                ExprKind::Function(call) => {
                    if call.filter.is_some() {
                        return Err(binder_error(
                            "FILTER clause is not yet supported for aggregates in USING KEY",
                        ));
                    }
                    if !call.order_by.is_empty() {
                        return Err(binder_error(
                            "ORDER BY clause is not yet supported for aggregates in USING KEY",
                        ));
                    }
                    if call.distinct {
                        return Err(binder_error(
                            "DISTINCT is not yet supported for aggregates in USING KEY",
                        ));
                    }
                    let set = super::expression::qualified_aggregate(&call.name).ok_or_else(|| cant_be_used(target))?;
                    let mut children = Vec::with_capacity(call.args.len());
                    for argument in &call.args {
                        // nothing plans a subquery over the rows of the CTE: one written here is refused
                        // before binding, and one that a macro expands to after it
                        let mut subquery = false;
                        visit_expr_mut(&mut argument.expr.clone(), &mut |e| {
                            subquery |= matches!(e.kind, ExprKind::Subquery { .. });
                        });
                        if subquery {
                            return Err(subquery_error());
                        }
                        let bound = binder.bind_expression(&argument.expr)?;
                        let mut subquery = false;
                        bound.clone().walk_mut(&mut |e| {
                            subquery |= matches!(e.kind, BoundExprKind::Subquery { .. });
                        });
                        if subquery {
                            return Err(subquery_error());
                        }
                        children.push(bound);
                    }
                    // the column the alias names, or else the column of the first argument
                    let column = if !target.alias.is_empty() {
                        names.iter().position(|n| *n == target.alias).ok_or_else(|| {
                            binder_error(format!(
                                "Could not find column with name {} to bind aggregate to.",
                                target.alias
                            ))
                        })?
                    } else {
                        children.first().and_then(column_of).ok_or_else(|| {
                            binder_error("In USING KEY, an aggregate must either have a column reference or an alias.")
                        })?
                    };
                    let aggregate = crate::function::binder::bind_aggregate_function(set, children, None, false)
                        .map_err(|error| {
                            if error.message.starts_with("No function matches") {
                                binder_error(format!("No matching aggregate function\n{}", error.message))
                            } else {
                                error
                            }
                        })?;
                    if payload.iter().any(|(c, _)| *c == column) {
                        return Err(binder_error(format!(
                            "Column {} referenced multiple times in USING KEY clause.\nTry using an alias for one \
                             of the aggregates.",
                            names[column]
                        )));
                    }
                    if keys.contains(&column) {
                        return Err(binder_error(format!(
                            "Column {} cannot be used as both key and aggregate in USING KEY clause.\nTry using an \
                             alias for the aggregation.",
                            names[column]
                        )));
                    }
                    return_types[column] = aggregate.return_type.clone();
                    payload.push((column, aggregate));
                }
                _ => return Err(cant_be_used(target)),
            }
        }
        self.next_table_index = binder.next_table_index;
        if keys.is_empty() {
            return Err(binder_error("USING KEY clause requires at least one key column."));
        }
        // every column that is neither a key nor aggregated keeps the value of its last row
        let last = crate::function::BuiltinFunctions::get()
            .aggregate("last")
            .expect("last is an aggregate");
        let mut aggregates = Vec::new();
        for (column, ty) in types.iter().enumerate() {
            if keys.contains(&column) {
                continue;
            }
            let aggregate = match payload.iter().position(|(c, _)| *c == column) {
                Some(at) => payload.swap_remove(at).1,
                None => {
                    let reference = BoundExpr::column_ref(ColumnBinding::new(table_index, column), ty.clone(), "");
                    crate::function::binder::bind_aggregate_function(last, vec![reference], None, false)?
                }
            };
            aggregates.push((column, aggregate));
        }
        // the aggregates read the rows of the CTE by position
        for (_, aggregate) in &mut aggregates {
            for child in &mut aggregate.children {
                child.walk_mut(&mut |e| {
                    if let BoundExprKind::ColumnRef { binding, .. } = &e.kind
                        && binding.table_index == table_index
                    {
                        e.kind = BoundExprKind::Reference(binding.column_index);
                    }
                });
            }
        }
        // `PlanRecursiveCTE`: collated keys group by their collated values, and collated results
        // compare by theirs
        let collation = self.default_collation();
        let collated = |source: BoundExpr, ty: &LogicalType| -> Result<Option<BoundExpr>> {
            let (collated, pushed) = crate::planner::collation::push_collation(
                source,
                ty,
                &collation,
                crate::planner::collation::CollationType::AllCollations,
            )?;
            Ok(pushed.then_some(collated))
        };
        let key_normalizers = keys
            .iter()
            .map(|&c| collated(BoundExpr::new(BoundExprKind::Reference(c), types[c].clone()), &types[c]))
            .collect::<Result<_>>()?;
        let payload_comparisons = aggregates
            .iter()
            .map(|(_, a)| {
                collated(
                    BoundExpr::new(BoundExprKind::Reference(0), a.return_type.clone()),
                    &a.return_type,
                )
            })
            .collect::<Result<_>>()?;
        let key = CteKey {
            keys,
            payload: aggregates,
            internal_types: types.to_vec(),
            key_normalizers,
            payload_comparisons,
            once: false,
        };
        Ok((key, return_types))
    }

    /// `Binder::BindNode(SetOperationNode)`.
    fn bind_set_operation(&mut self, node: &SetOperationNode, modifiers: &[ResultModifier]) -> Result<BoundStatement> {
        // `SetOpNeedsOrderWrap`: an ORDER BY expression over the result's columns that no leaf
        // selects is evaluated over the result: SELECT * FROM (set operation) ORDER BY ...
        fn simple(expr: &Expr) -> bool {
            match &expr.kind {
                ExprKind::ColumnRef(_)
                | ExprKind::Positional(_)
                | ExprKind::Constant(_)
                | ExprKind::Parameter(_)
                | ExprKind::Star(_) => true,
                ExprKind::Collate { child, .. } => simple(child),
                _ => false,
            }
        }
        fn provided(expr: &Expr, node: &QueryNode) -> bool {
            match &node.kind {
                QueryNodeKind::SetOperation(operation) => operation.children.iter().any(|c| provided(expr, c)),
                QueryNodeKind::Select(select) => {
                    let wanted = without_aliases(expr);
                    select.select_list.iter().any(|e| without_aliases(e) == wanted)
                }
                _ => true,
            }
        }
        let needs_wrap = modifiers.iter().any(|modifier| match modifier {
            ResultModifier::Order(orders) => orders
                .iter()
                .any(|order| !simple(&order.expr) && !node.children.iter().any(|child| provided(&order.expr, child))),
            _ => false,
        });
        if needs_wrap {
            let (distinct, outer): (Vec<ResultModifier>, Vec<ResultModifier>) = modifiers
                .iter()
                .cloned()
                .partition(|modifier| matches!(modifier, ResultModifier::Distinct(_)));
            let inner = QueryNode {
                kind: QueryNodeKind::SetOperation(node.clone()),
                modifiers: distinct,
                cte_map: Default::default(),
            };
            let select = SelectNode {
                select_list: vec![Expr::star()],
                from_table: TableRef::subquery(SelectStatement { node: inner }),
                ..SelectNode::default()
            };
            return self.bind_select_node(&select, &outer);
        }
        let mut children = Vec::with_capacity(node.children.len());
        for child in &node.children {
            let mut binder = self.child_binder();
            binder.ctes = self.ctes.clone();
            let bound = binder.bind_query_node(child);
            // an aggregate of an outer query's columns in a branch is the subquery's to move out
            if bound.is_err() && binder.pending_outer_aggregate.is_some() {
                self.pending_outer_aggregate = binder.pending_outer_aggregate.take();
            }
            let bound = bound?;
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
                // `CreateSetOperationJoin`: EXCEPT is an ANTI join and INTERSECT a SEMI join on all
                // columns, by their collations, with NULLs equal
                let mut plans = plans.into_iter();
                let (left, right) = (plans.next().expect("two children"), plans.next().expect("two children"));
                let plan = self.set_operation_join(
                    operation == SetOpType::Except,
                    node.setop_all,
                    &types,
                    left,
                    right,
                    table_index,
                )?;
                return self.finish_set_operation(plan, node, modifiers, &aliases, &types, table_index, names);
            }
        };
        let plan = LogicalOperator::new(kind, plans);
        self.finish_set_operation(plan, node, modifiers, &aliases, &types, table_index, names)
    }

    /// `VisitQueryNode` of a set operation: DISTINCT unless ALL, and the modifiers.
    #[allow(clippy::too_many_arguments)]
    fn finish_set_operation(
        &mut self,
        plan: LogicalOperator,
        node: &SetOperationNode,
        modifiers: &[ResultModifier],
        aliases: &[(String, usize)],
        types: &[LogicalType],
        table_index: usize,
        names: Vec<String>,
    ) -> Result<BoundStatement> {
        let mut prepared = Vec::with_capacity(modifiers.len() + 1);
        if !node.setop_all {
            prepared.push(PreparedModifier::Distinct);
        }
        for modifier in modifiers {
            prepared.push(self.prepare_set_operation_modifier(modifier, aliases, types, node)?);
        }
        let plan = self.apply_modifiers(plan, prepared, table_index, &names, types, types.len())?;
        Ok(BoundStatement {
            plan,
            column_aliases: aliases.to_vec(),
            names,
            types: types.to_vec(),
            return_type: StatementReturnType::QueryResult,
        })
    }

    /// `CreateSetOperationJoin`: the left rows with (EXCEPT: without) an equal right row, as a
    /// projection `table_index` of the left columns; with ALL the n-th copies of a row match.
    fn set_operation_join(
        &mut self,
        except: bool,
        all: bool,
        types: &[LogicalType],
        mut left: LogicalOperator,
        mut right: LogicalOperator,
        table_index: usize,
    ) -> Result<LogicalOperator> {
        let mut compared = types.to_vec();
        if all {
            left = self.add_row_number(types, left)?;
            right = self.add_row_number(types, right)?;
            compared.push(LogicalType::BIGINT);
        }
        let (left_bindings, right_bindings) = (left.column_bindings(), right.column_bindings());
        let collation = self.default_collation();
        let collate = |expr: BoundExpr, ty: &LogicalType| -> Result<BoundExpr> {
            Ok(crate::planner::collation::push_collation(
                expr,
                ty,
                &collation,
                crate::planner::collation::CollationType::AllCollations,
            )?
            .0)
        };
        let mut conditions = Vec::with_capacity(compared.len());
        for (i, ty) in compared.iter().enumerate() {
            conditions.push(crate::planner::logical::JoinCondition {
                bound: None,
                left: collate(BoundExpr::column_ref(left_bindings[i], ty.clone(), ""), ty)?,
                right: collate(BoundExpr::column_ref(right_bindings[i], ty.clone(), ""), ty)?,
                comparison: duckrs_parser::ast::ComparisonOp::NotDistinctFrom,
                filter: None,
            });
        }
        let join = LogicalOperator::new(
            LogicalOperatorKind::Join {
                join_type: if except {
                    duckrs_parser::ast::JoinType::Anti
                } else {
                    duckrs_parser::ast::JoinType::Semi
                },
                conditions,
                predicate: None,
                algorithm: Default::default(),
            },
            vec![left, right],
        );
        let expressions = types
            .iter()
            .enumerate()
            .map(|(i, ty)| BoundExpr::column_ref(left_bindings[i], ty.clone(), ""))
            .collect();
        Ok(LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index,
                expressions,
            },
            vec![join],
        ))
    }

    /// `AddRowNumber`: the rows numbered within the groups of rows equal in all columns.
    fn add_row_number(&mut self, types: &[LogicalType], child: LogicalOperator) -> Result<LogicalOperator> {
        use crate::planner::expression::{BoundWindow, WindowFunction};
        use duckrs_parser::ast::{WindowBoundary, WindowExclude};
        let bindings = child.column_bindings();
        let collation = self.default_collation();
        let mut partitions = Vec::with_capacity(types.len());
        for (i, ty) in types.iter().enumerate() {
            let column = BoundExpr::column_ref(bindings[i], ty.clone(), "");
            partitions.push(
                crate::planner::collation::push_collation(
                    column,
                    ty,
                    &collation,
                    crate::planner::collation::CollationType::AllCollations,
                )?
                .0,
            );
        }
        let window = BoundWindow {
            function: WindowFunction::RowNumber,
            aggregate: None,
            return_type: LogicalType::BIGINT,
            children: Vec::new(),
            filter: None,
            distinct: false,
            ignore_nulls: false,
            partitions,
            orders: Vec::new(),
            arg_orders: Vec::new(),
            start: WindowBoundary::UnboundedPreceding,
            end: WindowBoundary::UnboundedFollowing,
            start_expr: None,
            end_expr: None,
            exclude: WindowExclude::NoOther,
        };
        Ok(LogicalOperator::new(
            LogicalOperatorKind::Window {
                table_index: self.generate_table_index(),
                windows: vec![window],
            },
            vec![child],
        ))
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
        node: &SetOperationNode,
    ) -> Result<PreparedModifier> {
        // `OrderBinder` over the binders of the leaves: an expression one of them selects
        fn selected_by(expr: &Expr, node: &QueryNode, found: &mut Vec<usize>) {
            match &node.kind {
                QueryNodeKind::SetOperation(operation) => {
                    operation
                        .children
                        .iter()
                        .for_each(|child| selected_by(expr, child, found));
                }
                QueryNodeKind::Select(select) => {
                    let wanted = without_aliases(expr);
                    for (at, e) in select.select_list.iter().enumerate() {
                        if without_aliases(e) == wanted && !found.contains(&at) {
                            found.push(at);
                        }
                    }
                }
                _ => {}
            }
        }
        let operation = node;
        let ResultModifier::Order(nodes) = modifier else {
            let mut unused = Vec::new();
            return self.prepare_modifier(modifier, &mut unused, &mut SelectBindState::default());
        };
        if matches!(nodes.as_slice(), [node] if matches!(&node.expr.kind, ExprKind::Star(star) if star.expr.is_none()))
        {
            let node = &nodes[0];
            return Ok(PreparedModifier::OrderAll(
                self.options.resolve_order(node.order, node.nulls),
            ));
        }
        let mut orders = Vec::with_capacity(nodes.len());
        for node in nodes {
            // `COLUMNS('regex')`: each column of the set operation whose name matches
            if let ExprKind::Star(star) = &node.expr.kind
                && let Some(pattern) = &star.expr
            {
                let ExprKind::Constant(literal) = &pattern.kind else {
                    return Err(not_implemented(
                        "COLUMNS of a set operation's ORDER BY other than a regex",
                    ));
                };
                let regex =
                    regex::Regex::new(&literal.text).map_err(|e| Error::new(ErrorKind::InvalidInput, e.to_string()))?;
                let mut matched: Vec<usize> = aliases
                    .iter()
                    .filter(|(a, _)| regex.is_match(a))
                    .map(|(_, i)| *i)
                    .collect();
                matched.sort_unstable();
                matched.dedup();
                if matched.is_empty() {
                    return Err(binder_error(format!(
                        "No matching columns found that match regex \"{}\"",
                        literal.text
                    )));
                }
                for index in matched {
                    orders.push((self.options.resolve_order(node.order, node.nulls), index, None));
                }
                continue;
            }
            // `OrderBinder::TryGetProjectionReference`: another constant has no effect
            if let ExprKind::Constant(literal) = &node.expr.kind
                && literal.kind != LiteralKind::Integer
            {
                if self.options.order_by_non_integer_literal {
                    continue;
                }
                return Err(binder_error(
                    "ORDER BY non-integer literal has no effect.\n* SET order_by_non_integer_literal=true to allow \
                     this behavior.",
                ));
            }
            // `BindSelectNode`: a position with a collation
            let (order_expr, collation) = match &node.expr.kind {
                ExprKind::Collate { child, collation }
                    if matches!(&child.kind, ExprKind::Constant(l) if l.kind == LiteralKind::Integer)
                        || matches!(&child.kind, ExprKind::Positional(_)) =>
                {
                    (&**child, Some(collation.clone()))
                }
                _ => (&node.expr, None),
            };
            let index = match &order_expr.kind {
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
                    let by_alias = || {
                        aliases
                            .iter()
                            .find(|(a, _)| a.eq_ignore_ascii_case(&name))
                            .map(|(_, i)| *i)
                    };
                    // `projection_map`: an expression at different positions is ambiguous
                    let mut selected = Vec::new();
                    operation
                        .children
                        .iter()
                        .for_each(|child| selected_by(&node.expr, child, &mut selected));
                    let by_expression = match selected.as_slice() {
                        [index] if *index < types.len() => Some(*index),
                        [] | [_] => None,
                        _ if !matches!(node.expr.kind, ExprKind::ColumnRef(_)) => {
                            return Err(binder_error("Ambiguous reference to column"));
                        }
                        _ => None,
                    };
                    // `IsPotentialAlias`: only a plain name may name an alias first
                    let found = if matches!(&node.expr.kind, ExprKind::ColumnRef(names) if names.len() == 1) {
                        by_alias().or(by_expression)
                    } else {
                        by_expression.or_else(by_alias)
                    };
                    let Some(index) = found else {
                        return Err(binder_error(format!(
                            "Could not ORDER BY column \"{}\": add the expression/function to every SELECT, or move \
                             the UNION into a FROM clause.",
                            node.expr
                        )));
                    };
                    index
                }
            };
            if collation.is_some() && types.get(index).is_some_and(|t| t.id() != LogicalTypeId::Varchar) {
                return Err(binder_error("COLLATE can only be applied to varchar columns"));
            }
            orders.push((self.options.resolve_order(node.order, node.nulls), index, collation));
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
        // DISTINCT ON comes before the ORDER BY, which it takes over to keep the first row of each group as the ORDER
        // BY sees them (`LogicalDistinct::order_by`)
        let mut prepared = prepared;
        if let Some(distinct) = prepared
            .iter()
            .position(|m| matches!(m, PreparedModifier::DistinctOn(_)))
            && let Some(order) = prepared
                .iter()
                .position(|m| matches!(m, PreparedModifier::Order(_) | PreparedModifier::OrderAll(_)))
            && order < distinct
        {
            prepared.swap(distinct, order);
        }
        let mut kinds = Vec::with_capacity(prepared.len());
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
                    on: false,
                    orders: Vec::new(),
                },
                PreparedModifier::DistinctOn(indexes) => LogicalOperatorKind::Distinct {
                    targets: indexes.into_iter().map(collated_column).collect::<Result<_>>()?,
                    on: true,
                    orders: Vec::new(),
                },
                PreparedModifier::LimitPercent { percent, offset } => {
                    LogicalOperatorKind::LimitPercent { percent, offset }
                }
                PreparedModifier::Order(orders) => LogicalOperatorKind::Order {
                    orders: orders
                        .into_iter()
                        .map(|(modifiers, index, collation)| {
                            let expr = match collation {
                                Some(collation) => {
                                    let mut expr = column(index)?;
                                    expr.return_type = LogicalType::varchar_collation(&collation);
                                    let ty = expr.return_type.clone();
                                    let kind = crate::planner::collation::CollationType::AllCollations;
                                    crate::planner::collation::push_collation(expr, &ty, &default_collation, kind)?.0
                                }
                                None => collated_column(index)?,
                            };
                            Ok(BoundOrderByNode { modifiers, expr })
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
            kinds.push(kind);
        }
        if let Some(distinct) = kinds
            .iter()
            .position(|k| matches!(k, LogicalOperatorKind::Distinct { on: true, .. }))
            && let Some(LogicalOperatorKind::Order { orders }) = kinds[distinct + 1..]
                .iter()
                .find(|k| matches!(k, LogicalOperatorKind::Order { .. }))
        {
            let orders = orders.clone();
            if let LogicalOperatorKind::Distinct { orders: taken, .. } = &mut kinds[distinct] {
                *taken = orders;
            }
        }
        for kind in kinds {
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
            // `BoundColumnRefExpression` without an alias: the names are the statement's
            let expressions = (0..column_count)
                .map(|i| column(i).map(|c| c.with_alias(String::new())))
                .collect::<Result<_>>()?;
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
        // the columns of a struct UNNEST, whose names are its fields' rather than aliases of the select list
        let mut unnested_fields = Vec::new();
        // `NormalizeFilterStarExpressions`: before the expansion, so that a FILTER's star does not
        // repeat the expression around it
        let mut normalized = node.select_list.clone();
        for expr in &mut normalized {
            self.normalize_filter_stars(expr)?;
        }
        for expr in self.expand_star_expressions(&normalized)? {
            match self.struct_unnest_fields(&expr) {
                Some(fields) if fields.is_empty() => {
                    return Err(binder_error("UNNEST of an empty struct is not supported"));
                }
                Some(fields) => {
                    unnested_fields.extend(select_list.len()..select_list.len() + fields.len());
                    select_list.extend(fields);
                }
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
            // `BindPositionalReference`: the reference binds as the column, and is named after it
            if let ExprKind::Positional(position) = expr.kind
                && expr.alias.is_empty()
                && let Ok(column) = self.bind_context.position_to_column(position)
            {
                names.push(column.last().cloned().unwrap_or_default());
            } else {
                names.push(expression_name(expr));
            }
            let written = match (&expr.kind, expr.alias.is_empty()) {
                (ExprKind::ColumnRef(parts), true) => parts.last().cloned(),
                _ => None,
            };
            self.qualify_column_names(expr);
            // `ColumnQualifier::QualifyColumnName`: a column is named as the table spells it
            if let (Some(written), ExprKind::ColumnRef(parts)) = (written, &expr.kind)
                && let Some(column) = parts.last()
                && column.eq_ignore_ascii_case(&written)
            {
                *names.last_mut().expect("just pushed") = column.clone();
            }
            if !expr.alias.is_empty() && !unnested_fields.contains(&i) {
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
            // `NormalizeFilterStarExpressions`: the DISTINCT ON and ORDER BY targets too
            let mut modifier = modifier.clone();
            match &mut modifier {
                ResultModifier::Order(orders) => {
                    for order in orders {
                        self.normalize_filter_stars(&mut order.expr)?;
                    }
                }
                ResultModifier::Distinct(targets) => {
                    for target in targets {
                        self.normalize_filter_stars(target)?;
                    }
                }
                ResultModifier::Limit { .. } => {}
            }
            prepared.push(self.prepare_modifier(&modifier, &mut select_list, &mut state)?);
        }
        let where_clause = node
            .where_clause
            .as_deref()
            .map(|condition| self.bind_where_star_expression(condition))
            .transpose()?;
        // `BindSelectNode`: the sample is of the FROM clause, before the WHERE clause
        if let Some(options) = &node.sample {
            plan = bind_sample(options, plan)?;
        }
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
            // `ExpressionBinder::Bind` keeps the alias of what it binds
            let mut bound = bound?;
            if !expr.alias.is_empty() {
                bound.alias = expr.alias.clone();
            }
            expressions.push(bound);
            let aggregates = self.aggregates.as_mut().expect("set above");
            if node.force_aggregates && i < column_count {
                // GROUP BY ALL: an entry with columns becomes a group
                if !aggregates.bound_columns.is_empty() {
                    if aggregates.bound_aggregate {
                        return Err(binder_error("Cannot mix aggregates with non-aggregated columns!"));
                    }
                    // a window over aggregates is fine, a window over a group is not
                    if matches!(expr.kind, ExprKind::Window(_)) {
                        return Err(binder_error("Cannot group on a window clause"));
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
                // a collated group: grouped by the collated value, selected as `first(x)`; a volatile one is
                // grouped as it is, so that it is evaluated once
                let (collated, requires_collation) = if volatile_with_subqueries(&original) {
                    (original.clone(), false)
                } else {
                    crate::planner::collation::push_collation(
                        original.clone(),
                        &ty,
                        &default_collation,
                        crate::planner::collation::CollationType::AllCollations,
                    )?
                };
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
                let mut having = having.clone();
                self.normalize_filter_stars(&mut having)?;
                let having = self.qualified_with_aliases(&having, &state);
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
                let mut qualify = qualify.clone();
                self.normalize_filter_stars(&mut qualify)?;
                let qualify = self.qualified_with_aliases(&qualify, &state);
                let bound = self.bind_expression(&qualify);
                self.alias_fallback.clear();
                // `QualifyBinder::BindColumnRef`: a column that is neither in FROM nor an alias
                let bound = bound.map_err(|error| {
                    let missing = error
                        .message
                        .strip_prefix("Referenced column \"")
                        .and_then(|rest| rest.split_once("\" not found in FROM clause!"))
                        .map(|(name, _)| name.to_string());
                    match missing {
                        Some(name) => binder_error(format!(
                            "Referenced column {} not found in FROM clause and can't find in alias map.",
                            ast::sql_identifier(&name)
                        )),
                        None => error,
                    }
                });
                Some(bound?.cast_to(&LogicalType::BOOLEAN, false))
            }
            None => None,
        };
        let windows = self.windows.take().expect("set above").windows;
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
        // after the columns outside the groups (`BindSelectNode`)
        if qualify.is_some() && windows.is_empty() {
            return Err(binder_error(
                "at least one window function must appear in the SELECT column or QUALIFY clause",
            ));
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
                    perfect_hash: false,
                    domain_cap: None,
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
        // the hidden entries that ORDER BY and DISTINCT ON add are called what they compute
        let mut modifier_names = names.clone();
        modifier_names.extend(expressions.iter().skip(names.len()).map(|e| {
            if e.alias.is_empty() {
                e.to_string()
            } else {
                e.alias.clone()
            }
        }));
        let projection_index = self.generate_table_index();
        let plan = LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index: projection_index,
                expressions,
            },
            vec![plan],
        );
        let plan = self.apply_modifiers(plan, prepared, projection_index, &modifier_names, &types, column_count)?;
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
                    // `BindModifiers`: a star or COLUMNS stands for each of its columns
                    let mut has_star = false;
                    duckrs_parser::transformer::visit_expr_mut(&mut target.clone(), &mut |e: &mut Expr| {
                        has_star |= matches!(e.kind, ExprKind::Star(_));
                    });
                    let expanded = if has_star {
                        let mut expanded = Vec::new();
                        self.expand_star_expression(target, &mut expanded)?;
                        expanded
                    } else {
                        vec![target.clone()]
                    };
                    for target in &expanded {
                        indexes.push(self.bind_order_expression(target, select_list, state, "DISTINCT ON")?);
                    }
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
                    // `BindModifiers`: a star or COLUMNS orders by each column it stands for
                    let mut has_star = false;
                    duckrs_parser::transformer::visit_expr_mut(&mut node.expr.clone(), &mut |e: &mut Expr| {
                        has_star |= matches!(e.kind, ExprKind::Star(_));
                    });
                    let targets = if has_star {
                        let mut expanded = Vec::new();
                        self.expand_star_expression(&node.expr, &mut expanded)?;
                        expanded
                    } else {
                        vec![node.expr.clone()]
                    };
                    for target in &targets {
                        let index = self.bind_order_expression(target, select_list, state, "ORDER BY")?;
                        orders.push((self.options.resolve_order(node.order, node.nulls), index, None));
                    }
                }
                PreparedModifier::Order(orders)
            }
            ResultModifier::Limit { limit, offset, percent } => {
                if *percent {
                    let offset = match offset {
                        // the OFFSET of a LIMIT % is read when it runs, a parameter's too (`BoundLimitNode`)
                        Some(offset) => self.bind_limit_value(offset, true, true)?.unwrap_or(0),
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
                    match self.bind_limit_value(expr, is_offset, false) {
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
        binder.inline_ctes();
        let bound = binder.bind_expression(expr)?;
        // a subquery's value is read by the operator (`Value::GetValue`), whose cast fails as invalid input
        let mut has_subquery = false;
        bound.clone().walk_mut(&mut |e| {
            has_subquery |= matches!(e.kind, BoundExprKind::Subquery { .. });
        });
        let value = evaluate_scalar(&bound.cast_to(&LogicalType::DOUBLE, false)).map_err(|error| {
            if has_subquery && error.kind == ErrorKind::Conversion {
                Error::new(ErrorKind::InvalidInput, error.message)
            } else {
                error
            }
        })?;
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
    fn bind_limit_value(&mut self, expr: &Expr, is_offset: bool, runtime_parameter: bool) -> Result<Option<u64>> {
        let mut binder = self.child_binder();
        binder.inline_ctes();
        binder.correlated = Arc::default();
        let bound = binder.bind_expression(expr)?;
        if !binder.correlated.lock().is_empty() {
            return Err(binder_error("Correlated columns not supported in LIMIT/OFFSET"));
        }
        // `BindLimitValue`: a subquery is evaluated by the LIMIT operator, whose
        // `GetLimitValue` reads its value as an idx_t (`Value::GetValue`); so is a parameter where it is read then
        let mut has_subquery = runtime_parameter && super::macros::has_parameter(expr);
        bound.clone().walk_mut(&mut |e| {
            has_subquery |= matches!(e.kind, BoundExprKind::Subquery { .. });
        });
        if has_subquery {
            const MAX_LIMIT_VALUE: u64 = 1 << 62;
            // `Value::GetValueInternal`: a DECIMAL is read as a DOUBLE
            let bound = if bound.return_type.id() == LogicalTypeId::Decimal {
                bound.cast_to(&LogicalType::DOUBLE, false)
            } else {
                bound
            };
            let value = evaluate_scalar(&bound.cast_to(&LogicalType::UBIGINT, false)).map_err(|error| {
                if error.kind == ErrorKind::Conversion {
                    Error::new(ErrorKind::InvalidInput, error.message)
                } else {
                    error
                }
            })?;
            if value.is_null() {
                return Ok(if is_offset { Some(0) } else { None });
            }
            let value = value.as_i128().expect("a UBIGINT") as u64;
            if value > MAX_LIMIT_VALUE {
                return Err(binder_error(format!(
                    "Max value {value} for LIMIT/OFFSET is {MAX_LIMIT_VALUE}"
                )));
            }
            return Ok(Some(value));
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
        &mut self,
        expr: &Expr,
        select_list: &mut Vec<Expr>,
        state: &mut SelectBindState,
        component: &str,
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
                let index = self.bind_order_expression(child, select_list, state, component)?;
                match state.original_expressions.get(index) {
                    Some(entry) => {
                        let mut target = entry.clone();
                        target.alias.clear();
                        rewritten = Expr::from(ExprKind::Collate {
                            child: Box::new(target),
                            collation: collation.clone(),
                        })
                        .with_alias(
                            if matches!(&child.kind, ExprKind::Constant(_) | ExprKind::Positional(_)) {
                                ORDER_POSITION_COLLATE
                            } else {
                                ""
                            },
                        );
                        &rewritten
                    }
                    None => expr,
                }
            }
            _ => expr,
        };
        match &expr.kind {
            // with `order_by_non_integer_literal`, another constant orders by itself: no effect
            ExprKind::Constant(literal)
                if literal.kind != LiteralKind::Integer && self.options.order_by_non_integer_literal => {}
            // a parameter of a table macro being created, which is no literal
            ExprKind::Constant(literal) if literal.text == super::macros::CREATE_PARAMETER_MARK => {}
            ExprKind::Constant(literal) => {
                // ORDER BY an integer refers to a select list entry; other constants are an error
                if literal.kind != LiteralKind::Integer {
                    return Err(binder_error(format!(
                        "{component} non-integer literal has no effect.\n* SET order_by_non_integer_literal=true to \
                         allow this behavior."
                    )));
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
                    format!("Parameter not supported in {component} clause"),
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
        // `BindSelectNode`: a struct UNNEST expands only as an entry of the select list itself
        if self.struct_unnest_fields(&qualified).is_some() {
            return Err(binder_error(
                "UNNEST of struct cannot be used in ORDER BY/DISTINCT ON clause",
            ));
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
            || call.args.is_empty()
            || !call.args[0].name.is_empty()
        {
            return None;
        }
        let (max_depth, keep_parent_names) = self.unnest_options(call).ok()?;
        let argument = &call.args[0].expr;
        // inside the UNNEST, where another is refused
        self.unnest_level += 1;
        let bound = self.bind_expression(argument);
        self.unnest_level -= 1;
        let bound = bound.ok()?;
        let (list_unnests, struct_unnests) = unnest_depths(&bound.return_type, max_depth);
        if struct_unnests == 0 {
            return None;
        }
        // the list unnests, then a struct_extract per field of each struct, as far as the depth goes
        let mut base = argument.clone();
        for _ in 0..list_unnests {
            let mut unnest = call.clone();
            unnest.name.name = UNNEST_LEVEL.into();
            unnest.args = vec![ast::FunctionArgument::positional(base)];
            base = Expr::from(ExprKind::Function(unnest));
        }
        let mut expressions: Vec<(Expr, LogicalType, bool)> = {
            let ty = self.bind_expression(&base).ok()?.return_type;
            vec![(base, ty, false)]
        };
        for _ in 0..struct_unnests {
            let mut next = Vec::new();
            let mut expanded = false;
            for (expr, ty, extracted) in expressions {
                // an empty struct at the top expands to no columns at all
                if !extracted
                    && matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Tuple)
                    && ty.struct_children().is_empty()
                {
                    return Some(Vec::new());
                }
                if !matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Tuple) || ty.struct_children().is_empty() {
                    next.push((expr, ty, extracted));
                    continue;
                }
                expanded = true;
                let tuple = ty.id() == LogicalTypeId::Tuple;
                for (at, (name, child)) in ty.struct_children().iter().enumerate() {
                    let mut extract = call.clone();
                    extract.name.name = "struct_extract".into();
                    let key = if tuple {
                        Expr::constant(ast::Literal::integer((at + 1) as i64))
                    } else {
                        Expr::constant(ast::Literal::string(name.clone()))
                    };
                    let mut parent = expr.clone();
                    let parent_alias = std::mem::take(&mut parent.alias);
                    extract.args = vec![
                        ast::FunctionArgument::positional(parent),
                        ast::FunctionArgument::positional(key),
                    ];
                    let mut field = Expr::from(ExprKind::Function(extract));
                    field.alias = if tuple {
                        format!("element{}", at + 1)
                    } else if keep_parent_names && extracted {
                        format!("{parent_alias}.{name}")
                    } else {
                        name.clone()
                    };
                    next.push((field, child.clone(), true));
                }
            }
            expressions = next;
            if !expanded {
                break;
            }
        }
        Some(expressions.into_iter().map(|(e, _, _)| e).collect())
    }

    /// The options of `UNNEST(x, recursive := ..., max_depth := ..., keep_parent_names := ...)`: how many levels
    /// it unnests, and whether the columns of a struct keep the names of the structs they are in.
    pub(super) fn unnest_options(&mut self, call: &ast::FunctionExpr) -> Result<(usize, bool)> {
        let mut max_depth = 1usize;
        let mut keep_parent_names = false;
        if call.args.len() == 1 {
            return Ok((max_depth, keep_parent_names));
        }
        let mut supported = false;
        for argument in &call.args[1..] {
            if argument.name.is_empty() {
                break;
            }
            let alias = &argument.name;
            let bound = self.bind_expression(&argument.expr)?;
            let mut unnests = false;
            bound.clone().walk_mut(&mut |e| unnests |= is_unnest_call(e));
            if !bound.is_foldable() || unnests {
                return Err(binder_error(format!("UNNEST parameter \"{alias}\" must be a constant")));
            }
            let value = evaluate_scalar(&bound)?;
            if value.is_null() {
                return Err(binder_error(format!("UNNEST parameter \"{alias}\" cannot be NULL")));
            }
            let as_bool = |value: &Value| -> Result<bool> {
                let cast = crate::execution::parquet::cast_value(value, &LogicalType::BOOLEAN)?;
                Ok(matches!(cast.data(), duckrs_common::value::ValueData::Bool(true)))
            };
            match alias.to_lowercase().as_str() {
                "recursive" => {
                    if as_bool(&value)? {
                        max_depth = usize::MAX;
                    }
                }
                "max_depth" => {
                    let cast = crate::execution::parquet::cast_value(&value, &LogicalType::UINTEGER)?;
                    max_depth = match cast.data() {
                        duckrs_common::value::ValueData::UInt32(depth) => *depth as usize,
                        _ => 0,
                    };
                    if max_depth == 0 {
                        return Err(binder_error("UNNEST cannot have a max depth of 0"));
                    }
                }
                "keep_parent_names" => keep_parent_names = as_bool(&value)?,
                _ => return Err(binder_error(format!("Unsupported parameter \"{alias}\" for unnest"))),
            }
            supported = true;
        }
        if !supported {
            return Err(binder_error(
                "UNNEST - unsupported extra argument, unnest only supports recursive := [true/false], max_depth := # \
                 or keep_parent_names := [true/false]",
            ));
        }
        Ok((max_depth, keep_parent_names))
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
        // `t.*` of a table of a query around this one (a LATERAL's left side), or `s.*` of a STRUCT column of one:
        // its columns or fields, as correlated columns, with the star's modifiers
        if !star.relation_name.is_empty()
            && !self
                .bind_context
                .bindings
                .iter()
                .any(|b| b.alias.eq_ignore_ascii_case(&star.relation_name))
            && self.bind_context.matching_binding(&star.relation_name).ok().flatten().is_none()
        {
            let mut columns: Option<Vec<(String, Vec<String>)>> = None;
            for context in self.outer_contexts.iter().rev() {
                if let Some(binding) = context
                    .bindings
                    .iter()
                    .find(|b| b.alias.eq_ignore_ascii_case(&star.relation_name))
                {
                    columns = Some(
                        binding
                            .names
                            .iter()
                            .map(|name| (name.clone(), vec![binding.alias.clone(), name.clone()]))
                            .collect(),
                    );
                    break;
                }
                if let Some((binding, index)) = context.matching_binding(&star.relation_name)?
                    && binding.types[index].id() == LogicalTypeId::Struct
                {
                    columns = Some(
                        binding.types[index]
                            .struct_children()
                            .iter()
                            .map(|(field, _)| {
                                (
                                    field.clone(),
                                    vec![binding.alias.clone(), binding.names[index].clone(), field.clone()],
                                )
                            })
                            .collect(),
                    );
                    break;
                }
            }
            if let Some(columns) = columns {
                let mut excluded = vec![false; star.exclude_list.len()];
                for (name, path) in columns {
                    if let Some(position) = star.exclude_list.iter().position(|e| e.column.eq_ignore_ascii_case(&name)) {
                        excluded[position] = true;
                        continue;
                    }
                    let mut column = Expr::column(path);
                    column.alias = name.clone();
                    if let Some((alias, replacement)) =
                        star.replace_list.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name))
                    {
                        column = replacement.clone();
                        column.alias = alias.clone();
                    }
                    if let Some((_, alias)) = star
                        .rename_list
                        .iter()
                        .find(|(n, _)| n.column.eq_ignore_ascii_case(&name))
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
                return Err(BindContext::ambiguous_table(&star.relation_name, &found));
            }
            found
        };
        let in_scope = |qualification: &[String], binding: &Binding| match qualification.split_last() {
            Some((table, schema)) => {
                table.eq_ignore_ascii_case(&binding.alias)
                    && (schema.is_empty() || {
                        let path = schema.join(&crate::catalog::SCHEMA_PATH_SEPARATOR.to_string());
                        super::BindContext::schema_matches(binding, &path)
                    })
            }
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
        let before = self.bind_context.bindings.len();
        let plan = self.bind_unsampled_table_ref(table_ref)?;
        self.apply_setof_alias(table_ref, before);
        match &table_ref.sample {
            Some(options) => bind_sample(options, plan),
            None => Ok(plan),
        }
    }

    /// `ApplyPostgresSetofAliasCompatibility` and `AddPostgresSetofColumnAlias`: the one column of a
    /// set-returning function under an alias takes the alias as its name, and keeps the original
    /// name bindable (but out of `*`).
    fn apply_setof_alias(&mut self, table_ref: &TableRef, before: usize) {
        let TableRefKind::TableFunction {
            function,
            with_ordinality,
        } = &table_ref.kind
        else {
            return;
        };
        let ExprKind::Function(call) = &function.kind else {
            return;
        };
        let set_returning = call.name.qualification.is_empty()
            && matches!(
                call.name.name.to_lowercase().as_str(),
                "range" | "generate_series" | "unnest"
            );
        if !set_returning
            || table_ref.alias.is_empty()
            || !table_ref.column_name_alias.is_empty()
            || self.bind_context.bindings.len() != before + 1
        {
            return;
        }
        let binding = self.bind_context.bindings.last_mut().expect("just added");
        // the ordinality column comes after the function's own
        if binding.names.len() != 1 + usize::from(*with_ordinality) || !binding.generated.is_empty() {
            return;
        }
        let original = std::mem::replace(&mut binding.names[0], table_ref.alias.clone());
        if !original.eq_ignore_ascii_case(&table_ref.alias) {
            binding.virtual_columns.push((original, binding.types[0].clone(), 0));
        }
    }

    /// `WITH ORDINALITY`: the rows of the function, numbered from 1 in the order it makes them (per
    /// input row, for a lateral call), in a column `ordinality` (or `ordinality0`, ... when the
    /// function has a column of that name already).
    fn bind_with_ordinality(&mut self, function: &Expr, table_ref: &TableRef) -> Result<LogicalOperator> {
        let probe = parse_ast(&format!("SELECT * FROM {function}"))?.remove(0);
        let ast::Statement::Select(probe) = probe else {
            unreachable!("this text is a SELECT");
        };
        let mut binder = self.child_binder();
        binder.left_of_join = self.left_of_join.clone();
        let names = binder.bind_select_statement(&probe)?.names;
        let taken = |name: &str| {
            names
                .iter()
                .chain(&table_ref.column_name_alias)
                .any(|n| n.eq_ignore_ascii_case(name))
        };
        let mut ordinality = "ordinality".to_string();
        let mut suffix = 0;
        while taken(&ordinality) {
            ordinality = format!("ordinality{suffix}");
            suffix += 1;
        }
        let select = parse_ast(&format!(
            "SELECT *, row_number() OVER () AS {} FROM {function}",
            ast::sql_identifier(&ordinality)
        ))?
        .remove(0);
        let ast::Statement::Select(select) = select else {
            unreachable!("this text is a SELECT");
        };
        let name = match &function.kind {
            ExprKind::Function(call) => call.name.name.clone(),
            _ => "ordinality".to_string(),
        };
        self.bind_rewritten(&select, table_ref, &name)
    }

    pub(super) fn bind_unsampled_table_ref(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
        let TableRefKind::TableFunction { function, .. } = &table_ref.kind else {
            return self.bind_unsampled_table_ref_inner(table_ref);
        };
        let name = match &function.kind {
            ExprKind::Function(call) => call.name.name.to_lowercase(),
            _ => String::new(),
        };
        let saved = std::mem::replace(&mut self.table_function_name, name);
        let bound = self.bind_unsampled_table_ref_inner(table_ref);
        self.table_function_name = saved;
        bound
    }

    fn bind_unsampled_table_ref_inner(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
        let bound = self.bind_unsampled_table_ref_kind(table_ref);
        // `Binder::Bind(BaseTableRef)`: a table that does not exist is reported at the reference
        match (&table_ref.kind, table_ref.query_location.0, bound) {
            (TableRefKind::BaseTable { name, .. }, Some(location), Err(mut error))
                if error.position.is_none()
                    && error.kind == ErrorKind::Catalog
                    && error.message.starts_with(&format!("Table with name {} does not exist", name.name)) =>
            {
                error.position = Some(location);
                Err(error)
            }
            (_, _, bound) => bound,
        }
    }

    fn bind_unsampled_table_ref_kind(&mut self, table_ref: &TableRef) -> Result<LogicalOperator> {
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
                // `ReplacementScan::GetFullPath`: every component of the name, as a file name
                let full_path: String = name
                    .qualification
                    .iter()
                    .filter(|q| !q.is_empty())
                    .chain(std::iter::once(&name.name))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(".");
                let lower = full_path.to_lowercase();
                // a name a file reader would read is a file: with the local file system off, that is the error
                if self.options.local_file_system_disabled
                    && [".csv", ".tsv", ".json", ".jsonl", ".ndjson", ".parquet", ".geojson", ".geojsonl"]
                        .iter()
                        .any(|end| lower.trim_end_matches(".gz").trim_end_matches(".zst").ends_with(end))
                {
                    self.check_file_access(&full_path)?;
                }
                if [".csv", ".tsv", ".csv.gz", ".tsv.gz", ".csv.zst", ".tsv.zst"]
                    .iter()
                    .any(|end| lower.ends_with(end))
                    && (crate::execution::file_system::has_glob(&full_path)
                        || !crate::execution::file_system::glob_or_directory(&full_path, "csv").is_empty())
                {
                    let call = ast::FunctionExpr::new(
                        "read_csv_auto",
                        vec![Expr::constant(ast::Literal::string(full_path.clone()))],
                    );
                    let mut file_ref = table_ref.clone();
                    if file_ref.alias.is_empty() {
                        let file = full_path.rsplit('/').next().unwrap_or(&full_path);
                        file_ref.alias = file.split('.').next().unwrap_or(file).to_string();
                    }
                    return self.bind_read_csv(&call, &file_ref);
                }
                if [
                    ".json",
                    ".jsonl",
                    ".ndjson",
                    ".json.gz",
                    ".jsonl.gz",
                    ".ndjson.gz",
                    ".json.zst",
                    ".jsonl.zst",
                    ".ndjson.zst",
                    ".geojson",
                    ".geojsonl",
                    ".geojson.gz",
                    ".geojsonl.gz",
                    ".geojson.zst",
                    ".geojsonl.zst",
                ]
                .iter()
                .any(|end| lower.ends_with(end))
                    && !crate::execution::file_system::glob_or_directory(&full_path, "json").is_empty()
                {
                    let call = ast::FunctionExpr::new(
                        "read_json_auto",
                        vec![Expr::constant(ast::Literal::string(full_path.clone()))],
                    );
                    let mut file_ref = table_ref.clone();
                    if file_ref.alias.is_empty() {
                        let file = full_path.rsplit('/').next().unwrap_or(&full_path);
                        file_ref.alias = file.split('.').next().unwrap_or(file).to_string();
                    }
                    return self.bind_read_json(&call, &file_ref);
                }
                // `ReadDuckDBTableFunction::ReplacementScan`
                if name.qualification.is_empty()
                    && [".db", ".ddb", ".duckdb"]
                        .iter()
                        .any(|ext| lower.ends_with(ext) || lower.contains(&format!("{ext}?")))
                {
                    let call = ast::FunctionExpr::new(
                        "read_duckdb",
                        vec![Expr::constant(ast::Literal::string(name.name.clone()))],
                    );
                    let mut file_ref = table_ref.clone();
                    if file_ref.alias.is_empty() && !name.name.contains(['*', '?', '[']) {
                        file_ref.alias = name.name.rsplit('/').next().unwrap_or(&name.name).to_string();
                    }
                    return self.bind_read_duckdb(&call, &file_ref);
                }
                if lower.ends_with(".parquet")
                    && !crate::execution::file_system::glob_or_directory(&full_path, "parquet").is_empty()
                {
                    // `ParquetScanReplacement`
                    let call = ast::FunctionExpr::new(
                        "parquet_scan",
                        vec![Expr::constant(ast::Literal::string(full_path.clone()))],
                    );
                    let mut file_ref = table_ref.clone();
                    if file_ref.alias.is_empty() {
                        let file = full_path.rsplit('/').next().unwrap_or(&full_path);
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
                        working.types = working.recurring_types.clone();
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
                        catalog: None,
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
                // a table of a database file that `read_duckdb` opened
                let foreign = name.qualification.first().and_then(|first| {
                    self.foreign_databases
                        .iter()
                        .find(|f| f.name.eq_ignore_ascii_case(first))
                        .cloned()
                });
                // `Catalog::GetEntry` of another attached database: a statement that writes one database
                // reads the others by their names
                let other = match &foreign {
                    Some(_) => None,
                    None => self.other_database_of(name),
                };
                let entry = match (&foreign, &other) {
                    (Some(foreign), _) => {
                        foreign
                            .catalog
                            .get_entry(name.qualification.get(1).map(String::as_str), None, &name.name)
                    }
                    (None, Some((catalog, schema))) => catalog.get_entry(schema.as_deref(), None, &name.name),
                    (None, None) => self.catalog.get_entry(
                        self.schema_of(name)?.as_deref(),
                        self.search_schema.as_deref(),
                        &name.name,
                    ),
                };
                // `CatalogEntryRetriever::GetEntry`: past the search path of the statement's (or view's)
                // database comes that of the connection, in its default database
                let mut entry_catalog = self.catalog.name.clone();
                let entry = match entry {
                    Err(error) if foreign.is_none() && other.is_none() => {
                        // a schema path is looked for in the default database too
                        let schema = if name.qualification.is_empty() {
                            None
                        } else {
                            self.schema_of(name).ok().flatten()
                        };
                        match self.default_database_entry_in(schema.as_deref(), &name.name) {
                            Some(found) => {
                                if let Some(session) = &self.session {
                                    entry_catalog.clone_from(&session.default_database);
                                }
                                Ok(found)
                            }
                            None => Err(error),
                        }
                    }
                    entry => entry,
                };
                // `Catalog::TryLookupDefaultTable`: the bare name of a database that has a default table stands for
                // that table, once it is there; an entry of that name as well makes the name ambiguous
                if name.qualification.is_empty()
                    && foreign.is_none()
                    && let Some(session) = &self.session
                    && let Some(database) = (session.attached_catalog)(&name.name)
                    && let Some((schema, table)) = database.default_table()
                    && database.get_entry(Some(&schema), None, &table).is_ok()
                {
                    // (the catalog of an attached file goes by the name it was attached under, which only the
                    // database knows)
                    let default_name = [name.name.as_str(), &schema, &table].map(ast::sql_identifier).join(".");
                    if let Ok(found) = &entry {
                        let (kind, found_schema, found_name) = match found {
                            TableOrView::Table(table) => ("Table", &table.schema, &table.name),
                            TableOrView::View(view) => ("View", &view.schema, &view.name),
                        };
                        // (a nested schema is written as its path)
                        let found_qualified = std::iter::once(entry_catalog.as_str())
                            .chain(found_schema.split(crate::catalog::SCHEMA_PATH_SEPARATOR))
                            .chain(std::iter::once(found_name.as_str()))
                            .map(ast::sql_identifier)
                            .collect::<Vec<_>>()
                            .join(".");
                        let n = &name.name;
                        return Err(Error::new(
                            ErrorKind::Catalog,
                            format!(
                                "Ambiguity detected for '{n}': this could either refer to the '{kind}' '{n}', or the \
                                 attached catalog '{n}' which has a default table. To avoid this error, either detach \
                                 the catalog and reattach under a different name, or use a fully qualified name for \
                                 the '{kind}': '{found_qualified}' or for the Catalog Default Table: '{default_name}'."
                            ),
                        ));
                    }
                    let mut rewritten = table_ref.clone();
                    rewritten.kind = TableRefKind::BaseTable {
                        name: ast::QualifiedName {
                            qualification: vec![name.name.clone(), schema],
                            name: table,
                        },
                        at_clause: None,
                    };
                    return self.bind_unsampled_table_ref(&rewritten);
                }
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
                // the replacement scans the client registered (`DBConfig::replacement_scans`)
                if entry.is_err() && name.qualification.is_empty() && foreign.is_none() {
                    let scans = self
                        .session
                        .as_ref()
                        .map(|session| session.replacement_scans.clone())
                        .unwrap_or_default();
                    for scan in scans {
                        match scan(&name.name).transpose()? {
                            None => {}
                            Some(super::Replacement::Rows((names, types, chunks))) => {
                                return self.bind_chunks(table_ref, &name.name, names, types, chunks);
                            }
                            Some(super::Replacement::TableFunction { name: function, parameters }) => {
                                return self.bind_replacement_function(table_ref, &name.name, &function, &parameters);
                            }
                        }
                    }
                }
                // a bare name of a system table function binds the function, unless a view of the system
                // catalog has that name (`duckdb_databases` hides the internal databases)
                if entry.is_err()
                    && name.qualification.is_empty()
                    && Self::is_system_table(&name.name)
                    && crate::catalog::defaults::default_view(None, &name.name).is_none()
                {
                    return self
                        .bind_system_table(table_ref, &name.name)
                        .map(|op| table_function_rows(op, &name.name));
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
                    return self
                        .bind_log_table("duckdb_logs", true, table_ref)
                        .map(|op| table_function_rows(op, "duckdb_logs"));
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
                    TableOrView::Table(table) => {
                        self.note_read(&table.schema, &table.name);
                        table
                    }
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
                if table_ref.alias.is_empty() && foreign.is_none() {
                    binding.catalog = match &other {
                        Some(_) => name.qualification.first().cloned(),
                        None => self.session.as_ref().map(|session| session.database.clone()),
                    };
                }
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
                        zone_filters: Vec::new(),
                        optional_filters: Vec::new(),
                        index_rows: None,
                        sample: None,
                        extracts: Vec::new(),
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
                    catalog: None,
                    table_index,
                    names,
                    types: types.clone(),
                    row_id: None,
                    virtual_columns: Vec::new(),
                    generated: Vec::new(),
                });
                Ok(LogicalOperator::new(
                    LogicalOperatorKind::ExpressionGet {
                        function: None,
                        volatile: false,
                        materialized: false,
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
                    return self.bind_with_ordinality(function, table_ref);
                }
                // `Binder::BindTableFunctionParameters`
                if let ExprKind::Function(call) = &function.kind {
                    let named: Vec<bool> = call
                        .args
                        .iter()
                        .map(|a| !super::table_functions::named_argument(a).name.is_empty())
                        .collect();
                    if named.windows(2).any(|pair| pair[0] && !pair[1]) {
                        return Err(binder_error("Unnamed parameters cannot come after named parameters"));
                    }
                }
                // a call `system.main.f` became: the built-in, whatever macro has its name
                let system = std::mem::take(&mut self.system_table_function);
                if let ExprKind::Function(call) = &function.kind
                    && !system
                    && let Some(entry) = self.find_macro(&call.name, true)?
                {
                    return self.bind_table_macro(table_ref, call, entry);
                }
                // `system.main.f`: a table function of the system catalog, as an unqualified call that no
                // macro can shadow
                if let ExprKind::Function(call) = &function.kind
                    && let [catalog, schema] = call.name.qualification.as_slice()
                    && catalog.eq_ignore_ascii_case("system")
                    && (schema.eq_ignore_ascii_case(crate::catalog::DEFAULT_SCHEMA) || schema.eq_ignore_ascii_case("pg_catalog"))
                    && crate::catalog::defaults::default_macro(Some(schema), &call.name.name, true).is_none()
                    && CPP_TABLE_FUNCTIONS.contains(&call.name.name.to_lowercase().as_str())
                {
                    let mut unqualified = table_ref.clone();
                    if let TableRefKind::TableFunction { function, .. } = &mut unqualified.kind
                        && let ExprKind::Function(call) = &mut function.kind
                    {
                        call.name.qualification.clear();
                    }
                    let saved = std::mem::replace(&mut self.system_table_function, true);
                    let bound = self.bind_unsampled_table_ref(&unqualified);
                    self.system_table_function = saved;
                    return bound;
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
                            | "read_single_json_file"
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
                    && call.name.name.eq_ignore_ascii_case("read_duckdb")
                {
                    return self.bind_read_duckdb(call, table_ref);
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
                            | "parquet_full_metadata"
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
                    // `PragmaTableInfoBind`
                    let mut parts = super::expression::parse_qualified_name(&text)?;
                    let table = QualifiedName {
                        name: parts.pop().unwrap_or_default(),
                        qualification: parts,
                    };
                    return self
                        .bind_pragma_table_info(table_ref, &call.name.name, &table)
                        .map(|op| table_function_rows(op, &call.name.name));
                }
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && call.args.is_empty()
                    && Self::is_system_table(&call.name.name)
                {
                    return self
                        .bind_system_table(table_ref, &call.name.name)
                        .map(|op| table_function_rows(op, &call.name.name));
                }
                // a table function the client registered (`duckdb_register_table_function`)
                if let ExprKind::Function(call) = &function.kind
                    && call.name.qualification.is_empty()
                    && let Some(set) = self.user_table(&call.name.name)
                {
                    return self.bind_user_table(call, &set, table_ref);
                }
                self.bind_range(function, table_ref)
            }
            TableRefKind::Pivot(pivot) => self.bind_pivot(table_ref, pivot),
            TableRefKind::Show(show) => self.bind_show_ref(table_ref, show),
            TableRefKind::MatchRecognize(mr) => self.bind_match_recognize(table_ref, mr),
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
            catalog: None,
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
        self.note_read(&view.schema, &view.name);
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
        let mut bound = binder.bind_select_statement(&view.query)?;
        self.next_table_index = binder.next_table_index;
        if view.secure {
            // the boundary that keeps the optimizer out of the view
            let plan = std::mem::replace(
                &mut bound.plan,
                LogicalOperator::new(LogicalOperatorKind::CrossProduct, vec![]),
            );
            bound.plan = LogicalOperator::new(
                LogicalOperatorKind::SecureView {
                    view_name: view.name.clone(),
                    pushed_filters: Vec::new(),
                    propagate_statistics: false,
                    estimate: None,
                },
                vec![plan],
            );
        }
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
            return self
                .bind_duckdb_settings(call, table_ref)
                .map(|op| table_function_rows(op, &name));
        }
        if matches!(
            name.as_str(),
            "duckdb_secrets" | "duckdb_secret_types" | "duckdb_secret_type_parameters" | "which_secret"
        ) {
            return self
                .bind_secret_function(&name, call, table_ref)
                .map(|op| table_function_rows(op, &name));
        }
        if super::EXTERNAL_RESOURCE_FUNCTIONS.contains(&name.as_str()) {
            return self
                .bind_external_resource_function(&name, call, table_ref)
                .map(|op| table_function_rows(op, &name));
        }
        if name == "duckdb_logs" || name == "duckdb_log_contexts" {
            return self
                .bind_duckdb_logs(&name, call, table_ref)
                .map(|op| table_function_rows(op, &name));
        }
        if name == "pragma_metadata_info" {
            return self
                .bind_metadata_info(call, table_ref)
                .map(|op| table_function_rows(op, &name));
        }
        if name == "pragma_storage_info" {
            return self
                .bind_storage_info(call, table_ref)
                .map(|op| table_function_rows(op, &name));
        }
        // `DuckDBMemoryFunction`: a row per `MemoryTag`. duckrs does not account memory by tag (as
        // `pragma_database_size` reports no memory usage), so every tag shows no bytes.
        if name == "duckdb_memory" && call.args.is_empty() && call.name.qualification.iter().all(String::is_empty) {
            const MEMORY_TAGS: [&str; 16] = [
                "BASE_TABLE",
                "HASH_TABLE",
                "PARQUET_READER",
                "CSV_READER",
                "ORDER_BY",
                "ART_INDEX",
                "COLUMN_DATA",
                "METADATA",
                "OVERFLOW_STRINGS",
                "IN_MEMORY_TABLE",
                "ALLOCATOR",
                "EXTENSION",
                "TRANSACTION",
                "EXTERNAL_FILE_CACHE",
                "WINDOW",
                "OBJECT_CACHE",
            ];
            let rows = MEMORY_TAGS
                .iter()
                .map(|tag| vec![Value::varchar(*tag), Value::bigint(0), Value::bigint(0)])
                .collect();
            let names = ["tag", "memory_usage_bytes", "temporary_storage_bytes"]
                .map(str::to_string)
                .to_vec();
            let types = vec![LogicalType::VARCHAR, LogicalType::BIGINT, LogicalType::BIGINT];
            return self
                .bind_rows(table_ref, &name, names, types, rows)
                .map(|op| table_function_rows(op, &name));
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
            // `Catalog::GetEntry`: a name that is no table function of C++ either
            if !CPP_TABLE_FUNCTIONS.contains(&name.as_str()) {
                let mut best: Option<(f64, String)> = None;
                let candidates = CPP_TABLE_FUNCTIONS.iter().map(|n| n.to_string()).chain(
                    crate::catalog::defaults::default_macro_names()
                        .filter(|(_, _, table_macro)| *table_macro)
                        .map(|(_, n, _)| n.to_string()),
                );
                for candidate in candidates {
                    let score = duckrs_common::string_util::similarity_rating(&candidate, &call.name.name);
                    if score > best.as_ref().map_or(0.0, |(b, _)| *b) {
                        best = Some((score, candidate));
                    }
                }
                let hint = best.map_or_else(String::new, |(_, n)| format!("\nDid you mean \"{n}\"?"));
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!("Table Function with name {} does not exist!{hint}", call.name.name),
                ));
            }
            return Err(not_implemented(&format!("the table function {name}")));
        }
        if call.args.is_empty() || call.args.len() > 3 || call.args.iter().any(|a| !a.name.is_empty()) {
            return Err(not_implemented("this call of a range function"));
        }
        let mut arguments = Vec::with_capacity(call.args.len());
        for argument in &call.args {
            let mut binder = self.child_binder();
            binder.left_of_join = std::mem::take(&mut self.left_of_join);
            let bound = binder.bind_expression(&argument.expr);
            self.left_of_join = binder.left_of_join;
            arguments.push(bound?);
        }
        let mut values = Vec::with_capacity(call.args.len());
        let integral = arguments
            .iter()
            .all(|a| a.return_type.is_integral() || a.return_type.id() == LogicalTypeId::SqlNull);
        for bound in arguments {
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
        // `BindTableFunctionInternal`: aliases past the function's columns are ignored
        let mut names = vec![name];
        for (name, alias) in names.iter_mut().zip(&table_ref.column_name_alias) {
            name.clone_from(alias);
        }
        let table_index = self.generate_table_index();
        self.bind_context.add_binding(Binding {
            alias,
            schema: None,
            catalog: None,
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
                generate_series,
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
                .push((name.clone(), Arc::new(cte.clone()), self.outer_contexts.len(), None));
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
        let logical = !table.generated.is_empty();
        if logical {
            let names = table.logical_columns().iter().map(|c| c.name.to_string()).collect();
            let mut binding = super::generated::table_binding(table, alias, table_index, names);
            binding.schema = Some(table.schema.clone());
            // past the logical columns, and moved after the stored ones below
            binding.row_id =
                (self.returning_row_id && !self.merge_action_column).then(|| table.logical_columns().len());
            self.bind_context.add_binding(binding);
        } else {
            self.bind_context.add_binding(Binding {
                alias,
                // `schema.table.column` reads it too
                schema: Some(table.schema.clone()),
                catalog: None,
                table_index,
                names: table.columns.iter().map(|c| c.name.clone()).collect(),
                types: table.types(),
                row_id: self.returning_row_id.then_some(table.columns.len()),
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
        if list.is_empty() {
            return Err(binder_error("RETURNING list is empty!"));
        }
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
            // a virtual column: `*` left it out. Past the logical columns, if the binding has those.
            let columns = if logical {
                table.logical_columns().len()
            } else {
                table.columns.len()
            };
            let mut hidden: Vec<String> = (0..columns).map(|i| format!("\u{1}{i}")).collect();
            hidden.push("merge_action".to_string());
            let mut types = vec![LogicalType::SQLNULL; columns];
            types.push(LogicalType::VARCHAR);
            self.bind_context.add_binding(Binding {
                alias: "\u{1}merge".to_string(),
                schema: None,
                catalog: None,
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
            let row_id = columns.len();
            for e in &mut expressions {
                e.walk_mut(&mut |e| {
                    if let BoundExprKind::ColumnRef { binding, depth: 0 } = &mut e.kind
                        && binding.table_index == table_index
                    {
                        if binding.column_index == row_id {
                            binding.column_index = table.columns.len();
                        } else if let Some(Some(position)) = stored.get(binding.column_index) {
                            binding.column_index = *position;
                        }
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
                // `rowid` is a virtual column a DELETE can return
                binder.returning_row_id = true;
                let result = binder.bind_returning(plan, &delete.returning, &table, table_index, alias);
                binder.returning_row_id = false;
                return result;
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
                    // `UpdateBinder`
                    let errors = (
                        std::mem::replace(&mut binder.window_error, "window functions are not allowed in UPDATE"),
                        std::mem::replace(
                            &mut binder.aggregate_error,
                            "aggregate functions are not allowed in UPDATE",
                        ),
                    );
                    let bound = binder.bind_expression(value);
                    (binder.window_error, binder.aggregate_error) = errors;
                    bound?
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
                    // `InsertBinder`
                    let errors = (
                        std::mem::replace(
                            &mut self.window_error,
                            "INSERT statement cannot contain window functions!",
                        ),
                        std::mem::replace(&mut self.aggregate_error, "INSERT statement cannot contain aggregates!"),
                    );
                    let bound = self.bind_values(rows, Some(&expected_types), &defaults);
                    (self.window_error, self.aggregate_error) = errors;
                    let (types, expressions) = bound?;
                    let table_index = self.generate_table_index();
                    LogicalOperator::new(
                        LogicalOperatorKind::ExpressionGet {
                            table_index,
                            types,
                            expressions,
                            function: None,
                            volatile: false,
                            materialized: false,
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
                    // `ResolveInputProjection`: a projection of the query is only inlined when it
                    // forwards columns; otherwise the casts go in a projection of their own
                    let plan = self.resolve_input_projection(bound.plan, &bound.types, &expected_types);
                    // cast the query's columns to the column types
                    self.cast_logical_operator_to_types(&bound.types, &expected_types, plan)?
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

    /// `Binder::ResolveInputProjection`, of where the casts of an INSERT go: a projection at the root
    /// that computes more than column references gets a projection of its columns on top.
    fn resolve_input_projection(
        &mut self,
        op: LogicalOperator,
        source_types: &[LogicalType],
        target_types: &[LogicalType],
    ) -> LogicalOperator {
        let LogicalOperatorKind::Projection { expressions, .. } = &op.kind else {
            return op;
        };
        let inlinable = expressions
            .iter()
            .all(|e| matches!(e.kind, BoundExprKind::ColumnRef { .. }));
        if inlinable || source_types == target_types {
            return op;
        }
        let expressions = op
            .column_bindings()
            .into_iter()
            .zip(source_types)
            .map(|(binding, ty)| BoundExpr::column_ref(binding, ty.clone(), String::new()))
            .collect();
        let table_index = self.generate_table_index();
        LogicalOperator::new(
            LogicalOperatorKind::Projection {
                table_index,
                expressions,
            },
            vec![op],
        )
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
        let mut type_dependencies = Vec::new();
        self.take_used_types();
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
                if column.compression.is_some() {
                    return Err(binder_error(format!(
                        "Can't set a compression type on generated column {}, generated columns are not stored",
                        sql_quoted_identifier(&column.name)
                    )));
                }
                continue;
            }
            let Some(ty) = &column.column_type else {
                return Err(not_implemented("columns without a type"));
            };
            let mut definition = ColumnDefinition::new(column.name.clone(), self.bind_type(ty)?);
            type_dependencies.extend(self.take_used_types().into_iter().map(|t| (column.name.clone(), t)));
            definition.compression = self.column_compression(column, &definition.ty)?;
            if let Some(default) = &column.default_value {
                // `Binder::BindCreateTableInfo`: a stored DEFAULT can't refer to parameters
                // (unnamed `?` parameters are found missing before binding, as in C++)
                let mut named = false;
                duckrs_parser::transformer::visit_expr_mut(&mut default.clone(), &mut |e: &mut Expr| {
                    named |= matches!(&e.kind, ExprKind::Parameter(id) if !id.chars().all(|c| c.is_ascii_digit()));
                });
                if named {
                    return Err(binder_error("DEFAULT values cannot contain parameters"));
                }
                let mut binder = Binder::new(self.catalog);
                binder.aggregate_error = "DEFAULT value cannot contain aggregates!";
                binder.search_schema = self.search_schema.clone();
                // the sequences of `nextval` are those of the statement's databases
                binder.session = self.session.clone();
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
                type_dependencies,
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
                            type_dependencies: Vec::new(),
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
        // `ColumnList::SetAllowDuplicates`: without target names, a name taken becomes `name_1`, ...
        let allow_duplicates = info.columns.is_empty();
        let taken =
            |columns: &[ColumnDefinition], name: &str| columns.iter().any(|c| c.name.eq_ignore_ascii_case(name));
        for (i, (name, ty)) in bound.names.iter().zip(&bound.types).enumerate() {
            let mut name = info.columns.get(i).map_or(name, |c| &c.name).clone();
            if allow_duplicates {
                let base = name.clone();
                let mut index = 1;
                while taken(&columns, &name) {
                    name = format!("{base}_{index}");
                    index += 1;
                }
            } else if taken(&columns, &name) {
                return Err(Error::new(
                    ErrorKind::Catalog,
                    format!("Column with name {} already exists!", sql_quoted_identifier(&name)),
                ));
            }
            let name = &name;
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
/// `LogicalSample` (the physical plan refuses a BERNOULLI sample of a count: `execution::check_plan`).
fn bind_sample(options: &ast::SampleOptions, plan: LogicalOperator) -> Result<LogicalOperator> {
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

/// The name of an UNNEST the binder writes for one level of a deeper one, which may sit inside another.
pub(super) const UNNEST_LEVEL: &str = "__unnest_level";

/// How many levels of lists, then of structs, an UNNEST of `ty` that goes `max_depth` deep takes apart.
pub(super) fn unnest_depths(ty: &LogicalType, max_depth: usize) -> (usize, usize) {
    if ty.id() == LogicalTypeId::SqlNull {
        return (1, 0);
    }
    let mut ty = ty.clone();
    let mut lists = 0;
    while matches!(ty.id(), LogicalTypeId::List | LogicalTypeId::Array) {
        ty = ty.child_type().cloned().unwrap_or(LogicalType::SQLNULL);
        lists += 1;
        if lists >= max_depth {
            break;
        }
    }
    let structs = if matches!(ty.id(), LogicalTypeId::Struct | LogicalTypeId::Tuple) {
        max_depth - lists
    } else {
        0
    };
    (lists, structs)
}

/// The table functions of C++ (the core and the json, parquet and icu extensions). A name outside
/// this list is unknown to C++ as well, not merely unported.
pub(super) const CPP_TABLE_FUNCTIONS: &[&str] = &[
    "add_grammar_extension",
    "arrow_scan",
    "arrow_scan_dumb",
    "check_peg_parser",
    "checkpoint",
    "create_external_resource",
    "deregister_external_resource",
    "destroy_external_resource",
    "disable_logging",
    "disable_peg_parser",
    "disable_profiling",
    "duckdb_approx_database_count",
    "duckdb_available_metrics",
    "duckdb_columns",
    "duckdb_connection_count",
    "duckdb_constraints",
    "duckdb_coordinate_systems",
    "duckdb_databases",
    "duckdb_dependencies",
    "duckdb_dialects",
    "duckdb_eviction_queues",
    "duckdb_extension_repositories",
    "duckdb_extensions",
    "duckdb_external_file_cache",
    "duckdb_external_resource_types",
    "duckdb_external_resources",
    "duckdb_functions",
    "duckdb_grammar_extensions",
    "duckdb_indexes",
    "duckdb_keywords",
    "duckdb_live_query_metrics",
    "duckdb_log_contexts",
    "duckdb_logs",
    "duckdb_memory",
    "duckdb_optimizers",
    "duckdb_prepared_statements",
    "duckdb_schemas",
    "duckdb_secret_type_parameters",
    "duckdb_secret_types",
    "duckdb_secrets",
    "duckdb_sequences",
    "duckdb_settings",
    "duckdb_table_sample",
    "duckdb_tables",
    "duckdb_temporary_files",
    "duckdb_triggers",
    "duckdb_types",
    "duckdb_variables",
    "duckdb_views",
    "enable_logging",
    "enable_peg_parser",
    "enable_profiling",
    "force_checkpoint",
    "generate_series",
    "glob",
    "icu_calendar_names",
    "json_each",
    "json_execute_serialized_sql",
    "json_tree",
    "parquet_bloom_probe",
    "parquet_file_metadata",
    "parquet_full_metadata",
    "parquet_kv_metadata",
    "parquet_metadata",
    "parquet_page_index",
    "parquet_scan",
    "parquet_schema",
    "pg_timezone_names",
    "pragma_collations",
    "pragma_database_size",
    "pragma_metadata_info",
    "pragma_platform",
    "pragma_show",
    "pragma_storage_info",
    "pragma_table_info",
    "pragma_user_agent",
    "pragma_version",
    "query",
    "query_table",
    "range",
    "read_blob",
    "read_csv",
    "read_csv_auto",
    "read_duckdb",
    "read_json",
    "read_json_auto",
    "read_json_objects",
    "read_json_objects_auto",
    "read_ndjson",
    "read_ndjson_auto",
    "read_ndjson_objects",
    "read_parquet",
    "read_single_csv_file",
    "read_single_json_file",
    "read_single_parquet_file",
    "read_text",
    "register_external_resource",
    "register_external_resource_type",
    "remove_grammar_extension",
    "repeat",
    "repeat_row",
    "seq_scan",
    "sniff_csv",
    "sql_auto_complete",
    "summary",
    "test_all_types",
    "test_vector_types",
    "truncate_duckdb_logs",
    "unnest",
    "which_secret",
];

/// Rows a table function computed while binding: EXPLAIN shows them as a scan of `name`.
fn table_function_rows(mut op: LogicalOperator, name: &str) -> LogicalOperator {
    if let LogicalOperatorKind::ExpressionGet { function, .. } = &mut op.kind {
        *function = Some(name.to_lowercase());
    }
    op
}

impl Binder<'_> {
    /// `VerifyCompressionType`: the method `USING COMPRESSION` names for a column of a new table, if one, which the
    /// database's storage version has and which can store the column's type.
    pub(super) fn column_compression(&self, column: &ast::ColumnDefinition, ty: &LogicalType) -> Result<Option<u8>> {
        use crate::storage::compression::{compression_availability, compression_of_name, compression_supports};
        let Some(name) = column.compression else {
            return Ok(None);
        };
        let compression = compression_of_name(name).expect("the parser names a compression type");
        if let Err(deprecated) = compression_availability(compression, self.catalog.storage_version()) {
            return Err(binder_error(if deprecated {
                format!(
                    "Can't compress using user-provided compression type '{name}', that type is deprecated and only \
                     has decompress support"
                )
            } else {
                format!("Can't compress using user-provided compression type '{name}', that type is not available yet")
            }));
        }
        let physical = ty.physical_type();
        if !compression_supports(compression, physical) {
            return Err(binder_error(format!(
                "Can't compress column {} with type '{ty}' (physical: {}) using compression type '{name}'",
                sql_quoted_identifier(&column.name),
                physical.name()
            )));
        }
        Ok(Some(compression))
    }
}

/// `BoundSubqueryExpression::IsVolatile`: volatile, also through what the plans of its subqueries compute.
fn volatile_with_subqueries(expr: &BoundExpr) -> bool {
    fn plan_is_volatile(op: &LogicalOperator) -> bool {
        let mut probe = LogicalOperator::new(op.kind.clone(), Vec::new());
        probe.expressions_mut().into_iter().any(|e| volatile_with_subqueries(e)) || op.children.iter().any(plan_is_volatile)
    }
    if expr.is_volatile() {
        return true;
    }
    if let BoundExprKind::Subquery { subquery, .. } = &expr.kind
        && plan_is_volatile(&subquery.plan)
    {
        return true;
    }
    expr.children().into_iter().any(volatile_with_subqueries)
}
